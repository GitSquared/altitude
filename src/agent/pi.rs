use super::{
    AUTOPILOT_SYSTEM_PROMPT, Activity, AgentEngine, AgentRefusal, AgentStanding, AgentUsage,
    REPORT_SCHEMA, REPORT_SYSTEM_PROMPT, TaskProgressDecoder, TaskReportDecoder, TaskUsage,
    activity,
};
use crate::isolation::{
    DefaultFile, GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand, shell_quote,
};
use crate::task::{TaskAgentTurnContext, TaskAgentTurnReport};
use serde_json::Value;
use std::mem;
use uuid::Uuid;

// A major is a decision, not a schedule: the 72-hour environment refresh takes the
// newest 1.x on its own, and moving to 2.x is an edit someone makes here.
const INSTALL_COMMAND: &str = concat!(
    "npm install -g --ignore-scripts --no-fund --no-audit --no-progress --color=false ",
    "@earendil-works/pi-coding-agent@1",
    " && command -v pi >/dev/null"
);

const DEFAULT_SETTINGS: &str = r#"{
  "defaultProjectTrust": "always",
  "tuiMode": "fullscreen"
}
"#;

const DEFAULT_MCP: &str = r#"{
  "mcpServers": {
    "altitude": {
      "command": "python3",
      "args": ["/altitude/pi/altitude-bridge.py"],
      "exposure": "direct",
      "description": "Altitude's control plane for this task: its title, and the signals it and the operator exchange."
    }
  }
}
"#;

const REPORT_EXTENSION_PATH: &str = "/tmp/altitude-report.ts";
// What our own extension writes into the session. A session entry is data in a shape
// this side chose, which is the only kind of thing a refusal is read from.
const USAGE_ENTRY_TYPE: &str = "altitude.usage";
const USAGE_EXTENSION_PATH: &str = "/altitude/pi/altitude-usage.ts";
const REPORT_SCHEMA_ENV: &str = "ALTITUDE_REPORT_SCHEMA";
const REPORT_TOOL_NAME: &str = "altitude_task_report";
const REPORT_EXTENSION: &str = r#"import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const schema = JSON.parse(process.env.ALTITUDE_REPORT_SCHEMA ?? "{}");

const reportTool = defineTool({
	name: "altitude_task_report",
	label: "Altitude task report",
	description:
		"Return the required Altitude task report. It is the only permitted action in this turn.",
	parameters: Type.Unsafe(schema),
	async execute(_toolCallId, params) {
		return {
			content: [{ type: "text", text: "Altitude task report recorded." }],
			details: params,
			terminate: true,
		};
	},
});

export default function (pi: ExtensionAPI) {
	pi.registerTool(reportTool);
}
"#;

#[derive(Clone)]
pub struct PiCode;

pub struct PiStream {
    pending: String,
    report: Option<TaskAgentTurnReport>,
    settled: bool,
    model: Option<String>,
    error: Option<String>,
    // Pi prices every assistant message as it arrives, so the run's spend is their
    // sum. It says whether the price came from the provider or from its own table,
    // and one table-priced message makes the total an estimate.
    spent: AgentUsage,
    // The account's standing, as the extension left it in the session: which windows
    // are open, how much of each is gone, and when each comes back.
    standing: AgentStanding,
    // Whether the provider refused the turn. That much pi states plainly; everything
    // else about the refusal is in prose, which is not something to read a fact out of.
    refused: bool,
}

impl PiStream {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            report: None,
            settled: false,
            model: None,
            error: None,
            spent: AgentUsage {
                cost_from_provider: true,
                is_charged: true,
                ..AgentUsage::default()
            },
            standing: AgentStanding {
                provider: None,
                on_plan: None,
                observed_at_ms: 0,
                windows: Vec::new(),
            },
            refused: false,
        }
    }

    fn read_standing(&mut self, event: &Value) {
        let Some(entry) = event.get("entry") else {
            return;
        };
        if entry.get("customType").and_then(Value::as_str) != Some(USAGE_ENTRY_TYPE) {
            return;
        }
        let Ok(standing) = serde_json::from_value::<AgentStanding>(entry["data"].clone()) else {
            return;
        };
        self.standing = standing;
    }

    fn process_line(&mut self, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("tool_execution_end")
                if event.get("toolName").and_then(Value::as_str) == Some(REPORT_TOOL_NAME) =>
            {
                if event.get("isError").and_then(Value::as_bool) == Some(true) {
                    self.error = Some("Pi rejected the task report".to_owned());
                    return;
                }
                match event.pointer("/result/details") {
                    Some(details) => match serde_json::from_value(details.clone()) {
                        Ok(report) => self.report = Some(report),
                        Err(error) => {
                            self.error = Some(format!("Invalid Pi task report: {error}"));
                        }
                    },
                    None => self.error = Some("Pi's task report carried no details".to_owned()),
                }
            }
            // A run can end without a tool call, and Pi still exits successfully.
            // The assistant message is where that failure is visible.
            Some("message_end") => {
                let message = event.get("message");
                if message
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    != Some("assistant")
                {
                    return;
                }
                self.model = message
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(usage) = message.and_then(|message| message.get("usage")) {
                    self.spent.cost_usd_micros += usage
                        .pointer("/cost/total")
                        .and_then(Value::as_f64)
                        .map(|dollars| (dollars * 1_000_000.0).round() as i64)
                        .unwrap_or_default();
                    self.spent.total_tokens += usage
                        .get("totalTokens")
                        .and_then(Value::as_i64)
                        .unwrap_or_default();
                    self.spent.cost_from_provider &= usage
                        .pointer("/cost/totalFromProvider")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
                let stop_reason = message
                    .and_then(|message| message.get("stopReason"))
                    .and_then(Value::as_str);
                if stop_reason == Some("error") {
                    self.refused = true;
                }
                if matches!(stop_reason, Some("error") | Some("aborted")) {
                    let detail = message
                        .and_then(|message| message.get("errorMessage"))
                        .and_then(Value::as_str)
                        .unwrap_or("Pi ended the report turn without a reason");
                    self.error = Some(format!("Pi failed the report turn: {detail}"));
                }
            }
            Some("auto_retry_end")
                if event.get("success").and_then(Value::as_bool) == Some(false) =>
            {
                let detail = event
                    .get("finalError")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi gave up retrying the report turn");
                self.error = Some(format!("Pi failed the report turn: {detail}"));
            }
            Some("entry_appended") => self.read_standing(&event),
            Some("agent_settled") => self.settled = true,
            _ => {}
        }
    }
}

impl TaskReportDecoder for PiStream {
    fn take_standing(&mut self) -> Option<AgentStanding> {
        (!self.standing.is_empty()).then(|| self.standing.clone())
    }

    // A refusal is read from two facts the harness states outright: the provider
    // refused, and a window the extension reported is spent. Which window and when it
    // comes back are on that window, so nothing here depends on how the refusal was
    // worded or what language it was worded in.
    fn refusal(&mut self) -> Option<AgentRefusal> {
        if !self.refused {
            return None;
        }
        let spent = self.standing.first_to_clear()?;
        let clears_at_ms = spent.clears_at_ms?;
        Some(AgentRefusal::window(spent.window.clone(), clears_at_ms))
    }

    // The extension is the one who knows whether this credential is a plan, and it
    // only asks a subscription at all: a plan's prices are not money.
    fn take_usage(&mut self) -> Option<AgentUsage> {
        if self.spent.is_empty() {
            return None;
        }
        let mut spent = self.spent.clone();
        if self.standing.on_plan == Some(true) {
            spent.is_charged = false;
        }
        Some(spent)
    }

    fn failure_message(&mut self) -> Option<&str> {
        if !self.pending.is_empty() {
            let line = mem::take(&mut self.pending);
            self.process_line(&line);
        }
        self.error.as_deref()
    }

    fn process(&mut self, bytes: &[u8]) {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending.drain(..=newline).collect::<String>();
            self.process_line(&line);
        }
    }

    fn report(mut self: Box<Self>) -> Result<TaskAgentTurnReport, String> {
        if !self.pending.is_empty() {
            let line = mem::take(&mut self.pending);
            self.process_line(&line);
        }
        let mut report = self.report.take();
        if let Some(report) = &mut report {
            report.model = self.model.take();
        }
        let error = self.error.take();
        report.ok_or_else(|| {
            error.unwrap_or_else(|| {
                if self.settled {
                    "Pi's report turn settled without a task report"
                } else {
                    "Pi's report command ended without settling"
                }
                .to_owned()
            })
        })
    }
}

// The work turn is `--mode json` like the report turn, so what the agent does is
// already arriving; nothing has to be scraped out of a terminal.
pub struct PiProgress {
    pending: String,
    activity: Option<Activity>,
    // The assistant's text arrives a delta at a time; only the most recent block is
    // of any interest, and only far enough to read the first line of it.
    narration: String,
}

impl PiProgress {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            activity: None,
            narration: String::new(),
        }
    }

    fn process_line(&mut self, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        if event.get("type").and_then(Value::as_str) == Some("message_update") {
            {
                let update = event.get("assistantMessageEvent");
                let kind = update
                    .and_then(|update| update.get("type"))
                    .and_then(Value::as_str);
                let delta = update
                    .and_then(|update| update.get("delta"))
                    .and_then(Value::as_str);
                match kind {
                    Some("text_start") => self.narration.clear(),
                    Some("text_delta") => {
                        if let Some(delta) = delta {
                            push_preview(&mut self.narration, delta);
                        }
                    }
                    Some("text_end") => {
                        if let Some(said) = activity::narration(&self.narration) {
                            self.activity = Some(said);
                        }
                    }
                    Some("thinking_start") => self.narration.clear(),
                    Some("thinking_delta") => {
                        if let Some(delta) = delta {
                            push_preview(&mut self.narration, delta);
                        }
                    }
                    Some("thinking_end") => {
                        if let Some(thought) = activity::thinking(&self.narration) {
                            self.activity = Some(thought);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn push_preview(preview: &mut String, delta: &str) {
    if preview.chars().count() < activity::NARRATION_LIMIT {
        preview.push_str(delta);
    }
}

impl TaskProgressDecoder for PiProgress {
    fn process(&mut self, bytes: &[u8]) {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending.drain(..=newline).collect::<String>();
            self.process_line(&line);
        }
    }

    fn take_activity(&mut self) -> Option<Activity> {
        self.activity.take()
    }
}

impl AgentEngine for PiCode {
    fn autopilot_command(
        &self,
        conversation_id: &str,
        prompt: &str,
        context: &TaskAgentTurnContext,
        _resume: bool,
    ) -> String {
        // Pi creates the session file when it is absent and continues it when it
        // exists, so a work turn does not distinguish the two.
        format!(
            "exec pi --print --mode json --session {} --extension {} \
             --append-system-prompt {} {}",
            shell_quote(&self.conversation_transcript_path(conversation_id)),
            USAGE_EXTENSION_PATH,
            shell_quote(&super::system_prompt(AUTOPILOT_SYSTEM_PROMPT, context)),
            shell_quote(&format!(
                "{prompt}\n\nAltitude task context at the start of this turn: {context}"
            )),
        )
    }

    fn report_command(&self, conversation_id: &str, context: &TaskAgentTurnContext) -> String {
        // Only the report tool is enabled, so the report turn cannot do work
        // even when the task's own instructions ask for it.
        format!(
            "cat > {extension_path} <<'ALTITUDE_REPORT_EXTENSION'\n{extension}ALTITUDE_REPORT_EXTENSION\n\
             export {schema_env}={schema}\n\
             exec pi --print --mode json --session {session} --no-extensions \
             --extension {extension_path} --extension {usage_extension} --tools {tool} \
             --append-system-prompt {prompt} {message}",
            extension_path = REPORT_EXTENSION_PATH,
            usage_extension = USAGE_EXTENSION_PATH,
            extension = REPORT_EXTENSION,
            schema_env = REPORT_SCHEMA_ENV,
            schema = shell_quote(REPORT_SCHEMA),
            session = shell_quote(&self.conversation_transcript_path(conversation_id)),
            tool = REPORT_TOOL_NAME,
            prompt = shell_quote(&super::system_prompt(REPORT_SYSTEM_PROMPT, context)),
            message = shell_quote(&format!(
                "Return the required task report.\n\nAltitude task context at the start of this turn: {context}"
            )),
        )
    }

    fn operator_command(&self, conversation_id: &str, opening: &str) -> String {
        // The message is a positional one, so the session resumes with it as the first
        // thing the agent reads: no keystrokes into a terminal that is not ours.
        format!(
            "exec pi --session {} {}",
            shell_quote(&self.conversation_transcript_path(conversation_id)),
            shell_quote(opening)
        )
    }

    fn report_decoder(&self, _recorded: &TaskUsage) -> Box<dyn TaskReportDecoder> {
        Box::new(PiStream::new())
    }

    fn progress_decoder(&self) -> Option<Box<dyn TaskProgressDecoder>> {
        Some(Box::new(PiProgress::new()))
    }

    fn operator_requires_conversation(&self) -> bool {
        false
    }

    fn profile_recipe(&self) -> ProfileRecipe {
        ProfileRecipe {
            profile: "agents/pi".to_owned(),
            display: "Pi",
            sign_in: "Use /login in Pi to connect your provider.",
            // Pi has no sign-in command: the provider is chosen in its own interface.
            sign_in_command: SignInCommand::Interactive("exec pi"),
            // Pi keeps one credential per provider in auth.json, and writes an empty
            // object when there are none, so the question is whether it has any.
            status_command: "jq -e 'length > 0' /altitude/pi/auth.json >/dev/null \
                             && echo a provider is logged in",
            label: "installing Pi",
            setup_command: INSTALL_COMMAND,
            directories: vec![
                PersistentDirectory::root(
                    GuestPath::Absolute("/altitude/pi"),
                    include_str!("assets/pi.md"),
                    vec![
                        DefaultFile {
                            path: "settings.json",
                            contents: DEFAULT_SETTINGS,
                        },
                        DefaultFile {
                            path: "mcp.json",
                            contents: DEFAULT_MCP,
                        },
                    ],
                )
                .with_managed_file("altitude-bridge.py", include_str!("assets/bridge.py"))
                .with_managed_file("altitude-usage.ts", include_str!("assets/pi-usage.ts"))
                // Conversations live on the task's disk and travel with its
                // snapshot; the rest of the profile is shared across tasks.
                .with_task_local_subdirectories(&["sessions"]),
            ],
            environment: vec![
                ("PI_CODING_AGENT_DIR", "/altitude/pi"),
                ("PI_SKIP_VERSION_CHECK", "1"),
                ("PI_TELEMETRY", "0"),
            ],
        }
    }

    fn new_conversation_id(&self) -> String {
        Uuid::new_v4().to_string()
    }

    fn conversation_transcript_path(&self, conversation_id: &str) -> String {
        format!("/altitude/pi/sessions/{conversation_id}.jsonl")
    }
}
