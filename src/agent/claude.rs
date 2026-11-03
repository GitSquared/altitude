use super::{
    AUTOPILOT_SYSTEM_PROMPT, Activity, AgentEngine, REPORT_SCHEMA, REPORT_SYSTEM_PROMPT,
    TaskProgressDecoder, TaskReportDecoder, activity,
};
use crate::isolation::{
    DefaultFile, GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand, shell_quote,
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
    model: Option<String>,
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

const MCP_TOOLS: &str = "mcp__altitude__*";

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
    // Claude Code publishes no major-version channel of its own, so the major is
    // checked here rather than assumed: the vendor's stable pointer names a
    // release, the refresh takes the newest 2.x it names, and the day it names a
    // 3.x this build stops instead of taking the major for us.
    fn install_command() -> &'static str {
        "export HOME=/root\n\
         version=$(curl -fsSL https://downloads.claude.ai/claude-code-releases/stable)\n\
         case \"$version\" in\n\
           2.*) ;;\n\
           *) echo \"Claude Code's stable release is $version, not 2.x; decide the major here\" >&2\n\
              exit 1 ;;\n\
         esac\n\
         curl -fsSL https://claude.ai/install.sh -o /tmp/install-claude\n\
         bash /tmp/install-claude \"$version\"\n\
         test -x /root/.local/bin/claude\n\
         rm /tmp/install-claude"
    }
}

impl ClaudeStream {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            report: None,
            received_result: false,
            model: None,
            error: None,
        }
    }

    fn process_line(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        if message.get("type").and_then(Value::as_str) == Some("result") {
            self.received_result = true;
            self.model = message
                .get("model")
                .and_then(Value::as_str)
                .or_else(|| {
                    message
                        .get("modelUsage")
                        .and_then(Value::as_object)
                        .and_then(|usage| usage.keys().next().map(String::as_str))
                })
                .map(str::to_owned);
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
        let mut report = self.report.take();
        if let Some(report) = &mut report {
            report.model = self.model.take();
        }
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

// Claude names a tool in `content_block_start` before it has any arguments, and
// repeats it complete inside the assistant message; the complete one is what says
// what the tool is aimed at, so the start only fills the gap while arguments are
// still arriving.
pub struct ClaudeProgress {
    pending: String,
    activity: Option<Activity>,
    narration: String,
    // The same buffer holds what the model is saying and what it is thinking;
    // which of the two it is decides how the line reads when the block closes.
    thinking: bool,
}

impl ClaudeProgress {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            activity: None,
            narration: String::new(),
            thinking: false,
        }
    }

    fn process_line(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => {
                let event = message.get("event");
                match event
                    .and_then(|event| event.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("content_block_delta") => {
                        let delta = event.and_then(|event| event.get("delta"));
                        let kind = delta
                            .and_then(|delta| delta.get("type"))
                            .and_then(Value::as_str);
                        let speaking = matches!(kind, Some("text_delta") | Some("thinking_delta"));
                        if speaking
                            && let Some(text) = delta
                                .and_then(|delta| delta.get("text"))
                                .and_then(Value::as_str)
                            && self.narration.chars().count() < activity::NARRATION_LIMIT
                        {
                            // The same buffer holds what the model says and what it
                            // thinks; which one it was decides how the line reads.
                            self.thinking = kind == Some("thinking_delta");
                            self.narration.push_str(text);
                        }
                    }
                    Some("content_block_stop") => {
                        let said = if self.thinking {
                            activity::thinking(&self.narration)
                        } else {
                            activity::narration(&self.narration)
                        };
                        if let Some(said) = said {
                            self.activity = Some(said);
                        }
                        self.narration.clear();
                        self.thinking = false;
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                let blocks = message
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let text = blocks
                    .iter()
                    .rfind(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str);
                if let Some(said) = text.and_then(activity::narration) {
                    self.activity = Some(said);
                }
            }
            _ => {}
        }
    }
}

impl TaskProgressDecoder for ClaudeProgress {
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

    fn operator_command(&self, conversation_id: &str, opening: &str) -> String {
        format!(
            "exec claude --permission-mode auto --resume {} {}",
            shell_quote(conversation_id),
            shell_quote(opening)
        )
    }

    fn report_decoder(&self) -> Box<dyn TaskReportDecoder> {
        Box::new(ClaudeStream::new())
    }

    fn progress_decoder(&self) -> Option<Box<dyn TaskProgressDecoder>> {
        Some(Box::new(ClaudeProgress::new()))
    }

    fn operator_requires_conversation(&self) -> bool {
        true
    }

    fn profile_recipe(&self) -> ProfileRecipe {
        ProfileRecipe {
            profile: "agents/claude".to_owned(),
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
