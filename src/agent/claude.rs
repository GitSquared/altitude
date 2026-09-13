use super::AgentEngine;
use crate::isolation::{DefaultFile, EnvironmentRecipe, GuestPath, PersistentDirectory};
use serde::Deserialize;
use serde_json::Value;
use std::mem;
use uuid::Uuid;

#[derive(Clone)]
pub struct ClaudeCode;

#[derive(Clone, Debug, Deserialize)]
pub struct AutopilotDecision {
    pub next: AutopilotNext,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotNext {
    Continue,
    AwaitOperator,
    Complete,
}

pub struct ClaudeStream {
    pending: String,
    decision: Option<AutopilotDecision>,
}

const AUTOPILOT_SYSTEM_PROMPT: &str = "Altitude is driving this task without an operator watching. Continue autonomously whenever the next action is reasonably implied by the original task. At the end of this turn, choose `continue` if useful work remains that you can pursue from the original intent. Choose `await_operator` only when human review or an input is genuinely required. Choose `complete` only when the task is fully done and needs no further operator action.";
const AUTOPILOT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "next": { "enum": ["continue", "await_operator", "complete"] },
    "summary": { "type": "string" }
  },
  "required": ["next", "summary"]
}"#;

const DEFAULT_SETTINGS: &str = r#"{
  "tui": "fullscreen",
  "theme": "dark",
  "permissions": {
    "defaultMode": "auto"
  }
}
"#;

impl ClaudeCode {
    fn install_command() -> &'static str {
        "wget -qO /etc/apk/keys/claude-code.rsa.pub https://downloads.claude.ai/keys/claude-code.rsa.pub && \\
         printf '%s\\n' https://downloads.claude.ai/claude-code/apk/stable >> /etc/apk/repositories && \\
         apk add --no-cache claude-code"
    }

    pub fn autopilot_command(&self, conversation_id: &str, prompt: &str, resume: bool) -> String {
        let session = if resume {
            format!("--resume {}", shell_quote(conversation_id))
        } else {
            format!("--session-id {}", shell_quote(conversation_id))
        };
        format!(
            "exec claude -p --permission-mode auto \\
             --output-format stream-json --verbose --include-partial-messages \\
             --append-system-prompt {} --json-schema {} {session} {}",
            shell_quote(AUTOPILOT_SYSTEM_PROMPT),
            shell_quote(AUTOPILOT_SCHEMA),
            shell_quote(prompt),
        )
    }

    pub fn operator_command(&self, conversation_id: &str) -> String {
        format!(
            "exec claude --permission-mode auto --resume {}",
            shell_quote(conversation_id)
        )
    }
}

impl ClaudeStream {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            decision: None,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending.drain(..=newline).collect::<String>();
            self.process_line(&line);
        }
    }

    pub fn decision(mut self) -> Option<AutopilotDecision> {
        if !self.pending.is_empty() {
            let line = mem::take(&mut self.pending);
            self.process_line(&line);
        }
        self.decision
    }

    fn process_line(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        if message.get("type").and_then(Value::as_str) == Some("result")
            && let Some(output) = message.get("structured_output")
            && let Ok(decision) = serde_json::from_value(output.clone())
        {
            self.decision = Some(decision);
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\\"'\\\"'"))
}

impl AgentEngine for ClaudeCode {
    fn environment_recipe(&self) -> EnvironmentRecipe {
        EnvironmentRecipe {
            profile: "agents/claude",
            setup_command: Self::install_command(),
            directories: vec![PersistentDirectory::root(
                GuestPath::Absolute("/altitude/claude"),
                include_str!("assets/claude.md"),
                vec![DefaultFile {
                    path: "settings.json",
                    contents: DEFAULT_SETTINGS,
                }],
            )],
            environment: vec![
                ("CLAUDE_CONFIG_DIR", "/altitude/claude"),
                ("DISABLE_AUTOUPDATER", "1"),
            ],
        }
    }

    fn new_conversation_id(&self) -> String {
        Uuid::new_v4().to_string()
    }
}
