use crate::agent::{
    AUTOPILOT_SYSTEM_PROMPT, Activity, AgentEngine, AgentKind, AgentUsage, TaskProgressDecoder,
    TaskStreamDecoder, TaskUsage, activity, system_prompt,
};
use crate::isolation::{
    DefaultFile, GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand, shell_quote,
};
use crate::task::TaskAgentTurnContext;
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

#[derive(Clone)]
pub struct PiCode;

fn token_count(usage: &Value, field: &str) -> i64 {
    usage.get(field).and_then(Value::as_i64).unwrap_or_default()
}

pub struct PiStream {
    pending: String,
    model: Option<String>,
    error: Option<String>,
    // Pi prices every assistant message as it arrives, so the run's spend is their
    // sum. It says whether the price came from the provider or from its own table,
    // and one table-priced message makes the total an estimate.
    spent: AgentUsage,
}

impl PiStream {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            model: None,
            error: None,
            spent: AgentUsage {
                cost_from_provider: true,
                is_charged: true,
                ..AgentUsage::default()
            },
        }
    }

    fn process_line(&mut self, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
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
                    self.spent.cost_from_provider &= usage
                        .pointer("/cost/totalFromProvider")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // Pi names its token fields the way it names them in its own
                    // accounting; the run's totals are their sum over its messages.
                    self.spent.tokens.input += token_count(usage, "input");
                    self.spent.tokens.cache_write += token_count(usage, "cacheWrite");
                    self.spent.tokens.cache_read += token_count(usage, "cacheRead");
                    self.spent.tokens.output += token_count(usage, "output");
                }
                let stop_reason = message
                    .and_then(|message| message.get("stopReason"))
                    .and_then(Value::as_str);
                if matches!(stop_reason, Some("error") | Some("aborted")) {
                    let detail = message
                        .and_then(|message| message.get("errorMessage"))
                        .and_then(Value::as_str)
                        .unwrap_or("Pi ended the turn without a reason");
                    self.error = Some(format!("Pi failed the turn: {detail}"));
                }
            }
            Some("auto_retry_end")
                if event.get("success").and_then(Value::as_bool) == Some(false) =>
            {
                let detail = event
                    .get("finalError")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi gave up retrying the turn");
                self.error = Some(format!("Pi failed the turn: {detail}"));
            }
            _ => {}
        }
    }
}

impl TaskStreamDecoder for PiStream {
    fn take_usage(&mut self) -> Option<AgentUsage> {
        (!self.spent.is_unused()).then(|| self.spent.clone())
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

    fn take_model(&mut self) -> Option<String> {
        if !self.pending.is_empty() {
            let line = mem::take(&mut self.pending);
            self.process_line(&line);
        }
        self.model.take()
    }
}

// The turn is `--mode json`, so what the agent does is already arriving; nothing has
// to be scraped out of a terminal.
pub struct PiProgress {
    pending: String,
    activity: Option<Activity>,
    // The assistant's text and its reasoning arrive a delta at a time and share this
    // buffer; only the most recent block is of any interest.
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
                            self.narration.push_str(delta);
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
                            self.narration.push_str(delta);
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
            "exec pi --print --mode json --session {} \
             --append-system-prompt {} {}",
            shell_quote(&self.conversation_transcript_path(conversation_id)),
            shell_quote(&system_prompt(AUTOPILOT_SYSTEM_PROMPT, context)),
            shell_quote(&format!(
                "{prompt}\n\nAltitude task context at the start of this turn: {context}"
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

    fn stream_decoder(&self, _recorded: &TaskUsage) -> Box<dyn TaskStreamDecoder> {
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
            profile: AgentKind::Pi.profile().to_owned(),
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
