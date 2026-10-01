use super::AgentEngine;
use crate::isolation::{
    DefaultFile, EnvironmentRecipe, GuestPath, PersistentDirectory, shell_quote,
};
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

const AUTOPILOT_SYSTEM_PROMPT: &str = "Altitude is driving this task without an operator watching. Continue autonomously whenever the next action is reasonably implied by the original task. Match the precision of the operator's expressed intent. Investigate and take reasonable defaults instead of asking for confirmation. Altitude will ask for a structured task report after this work turn ends.";
const REPORT_SYSTEM_PROMPT: &str = r#"Altitude is driving this task without an operator watching. Do not perform more work in this turn. Return the structured task report now.

Keep routine reasoning, progress, recoverable failures, and decisions you can reasonably infer in the summary. Before raising an operator signal, weigh the cost of taking the operator's attention:
1. Is this necessary for the operator to know?
2. What action is expected, if any?
3. Is there enough time for the operator to respond?
4. Is the information genuinely new rather than repeated progress?
5. What happens if it is ignored?

Raise no signal unless that reasoning justifies one. Signals have strict meanings:
- update: unique, durable information worth seeing, with no operator action required. Choose continue.
- decision: new context makes the original intent genuinely ambiguous and no reasonable default remains. State what is missing, why no reasonable path can be inferred, and what happens if ignored. Choose await_operator. If independent useful work remains, continue it before requesting a blocking decision.
- review: the requested work and applicable machine verification are complete, and human acceptance or requested changes are now needed. Account for the whole expressed intent, including any unfinished or omitted work. Include concrete review evidence and verification limits; pending CI or automated review is still work in progress. Choose await_operator; never treat review readiness as operator acceptance.

Priority reflects response time and impact if ignored: low is useful whenever convenient; normal deserves routine attention; high should be handled before dependent work proceeds; urgent means delay risks serious or irreversible harm. Do not inflate priority.

Include every pull-request or merge-request URL already associated with this task in code_contribution_urls. Do not create or publish one merely to populate that field.

Choose complete only when the task needs neither operator review nor any other operator action."#;
const AUTOPILOT_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "requested_action": { "enum": ["continue", "await_operator", "complete"] },
    "summary": { "type": "string" },
    "operator_signal": {
      "anyOf": [
        { "type": "null" },
        {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "kind": { "enum": ["update", "decision", "review"] },
          "priority": { "enum": ["low", "normal", "high", "urgent"] },
          "title": { "type": "string" },
          "body": { "type": "string" }
        },
        "required": ["kind", "priority", "title", "body"]
        }
      ]
    },
    "code_contribution_urls": {
      "type": "array",
      "items": { "type": "string" }
    }
  },
  "required": ["requested_action", "summary", "operator_signal", "code_contribution_urls"]
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

    pub fn failure_message(&mut self) -> Option<&str> {
        if !self.pending.is_empty() {
            let line = mem::take(&mut self.pending);
            self.process_line(&line);
        }
        self.error.as_deref()
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
                        "Claude rejected the command"
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
