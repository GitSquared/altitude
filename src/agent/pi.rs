use super::{
    AUTOPILOT_SYSTEM_PROMPT, AgentEngine, REPORT_SCHEMA, REPORT_SYSTEM_PROMPT, TaskReportDecoder,
};
use crate::isolation::{
    DefaultFile, EnvironmentRecipe, GuestPath, PersistentDirectory, shell_quote,
};
use crate::task::{TaskAgentTurnContext, TaskAgentTurnReport};
use serde_json::Value;
use std::mem;
use uuid::Uuid;

/// Pi ships no standalone binary, so npm is its only distribution channel. The
/// version is pinned so an environment keeps the harness Altitude was tested
/// against instead of following whatever the registry publishes next.
const INSTALL_COMMAND: &str = concat!(
    "npm install -g --ignore-scripts --no-fund --no-audit --no-progress --color=false ",
    "@earendil-works/pi-coding-agent@0.99.1",
    " && test -x /usr/local/bin/pi"
);

/// Forced rather than left to the operator: the workspace is already isolated,
/// so trusting a project's own `.pi` resources prompts for nothing useful, and
/// the operator attaches to a terminal Altitude renders.
const DEFAULT_SETTINGS: &str = r#"{
  "defaultProjectTrust": "always",
  "tuiMode": "fullscreen"
}
"#;

/// Pi's view of Altitude's channel: the bridge this recipe writes as a managed
/// file, exposed directly so the model calls the tools without searching for
/// them. Which tools exist is decided at the binding point,
/// `TaskService::agent_tools`.
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

/// Where the report turn writes the generated extension inside the capsule.
const REPORT_EXTENSION_PATH: &str = "/tmp/altitude-report.ts";
/// The channel the schema travels on, so the report contract stays Altitude's.
const REPORT_SCHEMA_ENV: &str = "ALTITUDE_REPORT_SCHEMA";
/// The name the extension below registers. The shim repeats this name and
/// `REPORT_SCHEMA_ENV`; both mentions sit in this file, near each other.
const REPORT_TOOL_NAME: &str = "altitude_task_report";
/// Pi has no schema-forced output mode, so the report turn loads a tool that
/// carries Altitude's schema and terminates the turn with the report attached.
///
/// It is written per turn instead of living in the persistent profile: profile
/// defaults are only created once, and the schema must travel with Altitude's
/// code rather than with the operator's machine.
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
    error: Option<String>,
}

impl PiStream {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            report: None,
            settled: false,
            error: None,
        }
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
                let stop_reason = message
                    .and_then(|message| message.get("stopReason"))
                    .and_then(Value::as_str);
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
            Some("agent_settled") => self.settled = true,
            _ => {}
        }
    }
}

impl TaskReportDecoder for PiStream {
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
        let report = self.report.take();
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
            "exec pi --print --mode json --session {} \
             --append-system-prompt {} {}",
            shell_quote(&self.conversation_transcript_path(conversation_id)),
            shell_quote(AUTOPILOT_SYSTEM_PROMPT),
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
             --extension {extension_path} --tools {tool} \
             --append-system-prompt {prompt} {message}",
            extension_path = REPORT_EXTENSION_PATH,
            extension = REPORT_EXTENSION,
            schema_env = REPORT_SCHEMA_ENV,
            schema = shell_quote(REPORT_SCHEMA),
            session = shell_quote(&self.conversation_transcript_path(conversation_id)),
            tool = REPORT_TOOL_NAME,
            prompt = shell_quote(REPORT_SYSTEM_PROMPT),
            message = shell_quote(&format!(
                "Return the required task report.\n\nAltitude task context at the start of this turn: {context}"
            )),
        )
    }

    fn operator_command(&self, conversation_id: &str) -> String {
        format!(
            "exec pi --session {}",
            shell_quote(&self.conversation_transcript_path(conversation_id))
        )
    }

    fn report_decoder(&self) -> Box<dyn TaskReportDecoder> {
        Box::new(PiStream::new())
    }

    fn operator_requires_conversation(&self) -> bool {
        false
    }

    fn environment_recipe(&self) -> EnvironmentRecipe {
        EnvironmentRecipe {
            profile: "agents/pi",
            display: "Pi",
            command: "pi",
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
