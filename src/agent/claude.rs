use super::AgentEngine;
use crate::isolation::{DefaultFile, EnvironmentRecipe, GuestPath, PersistentDirectory};
use crate::tasks::{TaskAgentTurnContext, TaskAgentTurnReport};
use serde_json::Value;
use std::mem;
use uuid::Uuid;

#[derive(Clone)]
pub struct ClaudeCode;

pub struct ClaudeStream {
    pending: String,
    report: Option<TaskAgentTurnReport>,
    received_result: bool,
    error: Option<String>,
}

const AUTOPILOT_SYSTEM_PROMPT: &str = "Altitude is driving this task without an operator watching. Continue autonomously whenever the next action is reasonably implied by the original task. Do not stop for routine confirmation. Altitude will ask for a structured task report after this work turn ends.";
const REPORT_SYSTEM_PROMPT: &str = "Altitude is driving this task without an operator watching. Do not perform more work in this turn. Return the structured task report now. Set operator_attention_level to requested when you need an operator and choose await_operator, or alarm for a credible serious issue. Omit it when the task's current operator attention level remains appropriate; set it to nominal when an escalation is resolved. Use a nominal (informational) operator notice for a consequential judgment the operator may want to know about; keep working. Use requested or alarm operator notices to explain their corresponding operator attention level. An alarm may await_operator if no useful remediation or investigation remains. Choose complete only when the task is fully done and needs no further operator action.";
const AUTOPILOT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "requested_action": { "enum": ["continue", "await_operator", "complete"] },
    "operator_attention_level": { "enum": ["nominal", "requested", "alarm"] },
    "summary": { "type": "string" },
    "operator_notices": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "level": { "enum": ["nominal", "requested", "alarm"] },
          "title": { "type": "string" },
          "body": { "type": "string" }
        },
        "required": ["level", "title", "body"]
      }
    }
  },
  "required": ["requested_action", "summary", "operator_notices"]
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
        "export HOME=/root && \\
         curl -fsSL https://claude.ai/install.sh -o /tmp/install-claude && \\
         bash /tmp/install-claude && \\
         test -x /root/.local/bin/claude && \\
         rm /tmp/install-claude"
    }

    pub fn autopilot_command(
        &self,
        conversation_id: &str,
        prompt: &str,
        context: &TaskAgentTurnContext,
        resume: bool,
    ) -> String {
        let session = if resume {
            format!("--resume {}", shell_quote(conversation_id))
        } else {
            format!("--session-id {}", shell_quote(conversation_id))
        };
        format!(
            "exec claude -p --permission-mode auto \\
             --output-format stream-json --verbose --include-partial-messages \\
             --system-prompt-snapshot off \\
             --append-system-prompt {} {session} {}",
            shell_quote(AUTOPILOT_SYSTEM_PROMPT),
            shell_quote(&format!(
                "{prompt}\n\nAltitude task context at the start of this turn: {context}"
            )),
        )
    }

    pub fn report_command(&self, conversation_id: &str, context: &TaskAgentTurnContext) -> String {
        format!(
            "exec claude -p --permission-mode auto --output-format json \\
             --system-prompt-snapshot off \\
             --append-system-prompt {} --json-schema {} --resume {} {}",
            shell_quote(REPORT_SYSTEM_PROMPT),
            shell_quote(AUTOPILOT_SCHEMA),
            shell_quote(conversation_id),
            shell_quote(&format!(
                "Return the required task report.\n\nAltitude task context at the start of this turn: {context}"
            )),
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
            report: None,
            received_result: false,
            error: None,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending.drain(..=newline).collect::<String>();
            self.process_line(&line);
        }
    }

    pub fn report(mut self) -> Result<TaskAgentTurnReport, String> {
        if !self.pending.is_empty() {
            let line = mem::take(&mut self.pending);
            self.process_line(&line);
        }
        self.report.ok_or_else(|| {
            self.error.unwrap_or_else(|| {
                if self.received_result {
                    "Claude's report result did not contain structured output"
                } else {
                    "Claude's report command ended without a result response"
                }
                .to_owned()
            })
        })
    }

    fn process_line(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        if message.get("type").and_then(Value::as_str) == Some("result") {
            self.received_result = true;
            if message.get("is_error").and_then(Value::as_bool) == Some(true) {
                self.error = Some(
                    if message
                        .get("result")
                        .and_then(Value::as_str)
                        .is_some_and(|result| result.contains("Not logged in"))
                    {
                        "Claude is not logged in in its sandbox profile; open an attached Claude shell and run /login"
                    } else {
                        "Claude rejected the report request"
                    }.to_owned(),
                );
            }
            if let Some(output) = message.get("structured_output") {
                match serde_json::from_value(output.clone()) {
                    Ok(report) => self.report = Some(report),
                    Err(error) => self.error = Some(format!("Invalid Claude task report: {error}")),
                }
            }
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

impl AgentEngine for ClaudeCode {
    fn environment_recipe(&self) -> EnvironmentRecipe {
        EnvironmentRecipe {
            profile: "agents/claude",
            setup_command: Self::install_command(),
            directories: vec![
                PersistentDirectory::root(
                    GuestPath::Absolute("/altitude/claude"),
                    include_str!("assets/claude.md"),
                    vec![DefaultFile {
                        path: "settings.json",
                        contents: DEFAULT_SETTINGS,
                    }],
                )
                .with_task_local_subdirectories(&[
                    "projects",
                    "file-history",
                    "plans",
                    "session-env",
                    "tasks",
                    "paste-cache",
                    "image-cache",
                    "uploads",
                    "shell-snapshots",
                    "sessions",
                    "debug",
                    "feedback",
                    "feedback-bundles",
                    "usage-data",
                    "todos",
                ]),
            ],
            environment: vec![
                ("CLAUDE_CONFIG_DIR", "/altitude/claude"),
                ("DISABLE_AUTOUPDATER", "1"),
                (
                    "PATH",
                    "/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                ),
            ],
        }
    }

    fn new_conversation_id(&self) -> String {
        Uuid::new_v4().to_string()
    }

    fn conversation_transcript_path(&self, conversation_id: &str) -> String {
        format!("/altitude/claude/projects/-/{conversation_id}.jsonl")
    }
}
