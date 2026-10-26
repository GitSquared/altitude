use super::{
    AUTOPILOT_SYSTEM_PROMPT, AgentEngine, REPORT_SCHEMA, REPORT_SYSTEM_PROMPT, TaskReportDecoder,
};
use crate::isolation::{
    DefaultFile, EnvironmentRecipe, GuestPath, PersistentDirectory, SignInCommand, shell_quote,
};
use crate::task::{TaskAgentTurnContext, TaskAgentTurnReport};
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

const DEFAULT_SETTINGS: &str = r#"{
  "tui": "fullscreen",
  "theme": "dark",
  "permissions": {
    "defaultMode": "auto"
  }
}
"#;

/// The MCP tools Claude may call without asking, which are Altitude's channel.
const MCP_TOOLS: &str = "mcp__altitude__*";

/// Claude's view of Altitude's channel: the bridge this recipe writes as a
/// managed file, named by `--mcp-config` on every turn. Which tools exist is
/// decided at the binding point, `TaskService::agent_tools`.
const DEFAULT_MCP: &str = r#"{
  "mcpServers": {
    "altitude": {
      "command": "python3",
      "args": ["/altitude/claude/altitude-bridge.py"]
    }
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

impl TaskReportDecoder for ClaudeStream {
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
                if self.received_result {
                    "Claude's report result did not contain structured output"
                } else {
                    "Claude's report command ended without a result response"
                }
                .to_owned()
            })
        })
    }
}

impl AgentEngine for ClaudeCode {
    fn autopilot_command(
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
             --mcp-config /altitude/claude/mcp.json \
             --allowedTools {} \
             --append-system-prompt {} {session} {}",
            shell_quote(MCP_TOOLS),
            shell_quote(&super::system_prompt(AUTOPILOT_SYSTEM_PROMPT, context)),
            shell_quote(&format!(
                "{prompt}\n\nAltitude task context at the start of this turn: {context}"
            )),
        )
    }

    fn report_command(&self, conversation_id: &str, context: &TaskAgentTurnContext) -> String {
        format!(
            "exec claude -p --permission-mode auto --output-format json \\
             --system-prompt-snapshot off \\
             --mcp-config /altitude/claude/mcp.json \
             --allowedTools {} \
             --append-system-prompt {} --json-schema {} --resume {} {}",
            shell_quote(MCP_TOOLS),
            shell_quote(&super::system_prompt(REPORT_SYSTEM_PROMPT, context)),
            shell_quote(REPORT_SCHEMA),
            shell_quote(conversation_id),
            shell_quote(&format!(
                "Return the required task report.\n\nAltitude task context at the start of this turn: {context}"
            )),
        )
    }

    fn operator_command(&self, conversation_id: &str) -> String {
        format!(
            "exec claude --permission-mode auto --resume {}",
            shell_quote(conversation_id)
        )
    }

    fn report_decoder(&self) -> Box<dyn TaskReportDecoder> {
        Box::new(ClaudeStream::new())
    }

    fn operator_requires_conversation(&self) -> bool {
        true
    }

    fn environment_recipe(&self) -> EnvironmentRecipe {
        EnvironmentRecipe {
            profile: "agents/claude",
            display: "Claude Code",
            sign_in: "Follow Claude Code's sign-in instructions.",
            // The sign-in is a command inside Claude Code's own interface, so there
            // is nothing to run but Claude Code itself.
            sign_in_command: SignInCommand::Interactive("exec claude"),
            // Claude Code writes this file when it is signed in, and its own CLI has
            // no way to be asked. Existence and size only: the contents are the
            // credential, and Altitude does not read it.
            status_command: "test -s /altitude/claude/.credentials.json && echo a login is here",
            label: "installing Claude Code",
            setup_command: Self::install_command(),
            directories: vec![
                PersistentDirectory::root(
                    GuestPath::Absolute("/altitude/claude"),
                    include_str!("assets/claude.md"),
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
