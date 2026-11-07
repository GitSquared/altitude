use crate::agent::{
    AUTOPILOT_SYSTEM_PROMPT, Activity, AgentEngine, AgentKind, AgentRefusal, AgentStanding,
    AgentUsage, AgentWindow, AgentWindowReading, TaskProgressDecoder, TaskStreamDecoder,
    TaskTokens, TaskUsage, activity, system_prompt,
};
use crate::isolation::{
    DefaultFile, GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand, shell_quote,
};
use crate::task::TaskAgentTurnContext;
use crate::vcs::now_ms;
use serde_json::Value;
use std::mem;
use uuid::Uuid;

#[derive(Clone)]
pub struct ClaudeCode;

fn token_count(usage: &Value, field: &str) -> i64 {
    usage.get(field).and_then(Value::as_i64).unwrap_or_default()
}

pub struct ClaudeStream {
    pending: String,
    received_result: bool,
    model: Option<String>,
    error: Option<String>,
    // The newest thing Claude said about the account's limits. A run reports them
    // while it works and again when it is refused, so the last word is the one that
    // says whether the refusal is the end.
    limit: Option<Value>,
    cost_usd_micros: i64,
    tokens: TaskTokens,
    // Claude Code counts the conversation, not the run, so what this run spent is
    // what the conversation has reached less what it had reached before.
    recorded: TaskUsage,
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
    // A plan's prices are list prices: nothing is being paid unless the account is
    // spending usage credits on top of it. A session that reported no windows at all
    // is an API key or a cloud provider, whose number is what it is.
    fn is_charged(&self) -> bool {
        let Some(limit) = self.limit.as_ref() else {
            return true;
        };
        let on_plan = limit
            .get("unifiedWindows")
            .and_then(Value::as_object)
            .is_some_and(|windows| !windows.is_empty());
        let using_overage = limit
            .get("isUsingOverage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        !on_plan || using_overage
    }

    pub fn new(recorded: &TaskUsage) -> Self {
        Self {
            pending: String::new(),
            received_result: false,
            model: None,
            error: None,
            limit: None,
            cost_usd_micros: 0,
            tokens: TaskTokens::default(),
            recorded: recorded.clone(),
        }
    }

    fn process_line(&mut self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        if message.get("type").and_then(Value::as_str) == Some("rate_limit_event")
            && let Some(info) = message.get("rate_limit_info")
        {
            self.limit = Some(info.clone());
        }
        if message.get("type").and_then(Value::as_str) == Some("result") {
            self.received_result = true;
            self.cost_usd_micros = message
                .get("total_cost_usd")
                .and_then(Value::as_f64)
                .map(|dollars| (dollars * 1_000_000.0).round() as i64)
                .unwrap_or_default();
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
            // The result counts the whole conversation, like its cost, so this is
            // the running total a later run subtracts from.
            if let Some(usage) = message.get("usage") {
                self.tokens = TaskTokens {
                    input: token_count(usage, "input_tokens"),
                    cache_write: token_count(usage, "cache_creation_input_tokens"),
                    cache_read: token_count(usage, "cache_read_input_tokens"),
                    output: token_count(usage, "output_tokens"),
                };
            }
            if message.get("is_error").and_then(Value::as_bool) == Some(true) {
                // What Claude said is the best account of why the turn ended, so the
                // operator gets it rather than a sentence about an exit code. A login
                // that has lapsed is the one case worth naming, because the fix is a
                // command.
                let said = message
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default();
                self.error = Some(if said.contains("Not logged in") {
                    "Claude is not logged in in its sandbox profile; open an attached \
                     Claude shell and run /login"
                        .to_owned()
                } else if said.is_empty() {
                    "Claude rejected the turn".to_owned()
                } else {
                    format!("Claude rejected the turn: {said}")
                });
            }
        }
    }
}

impl TaskStreamDecoder for ClaudeStream {
    // Every window Claude tracks, on every observation, not only the one that is
    // limiting: `unifiedWindows` is the account's standing and the top-level fields
    // are only whichever window is currently the tightest.
    fn take_standing(&mut self) -> Option<AgentStanding> {
        if !self.pending.is_empty() {
            let line = mem::take(&mut self.pending);
            self.process_line(&line);
        }
        let limit = self.limit.as_ref()?;
        let windows: Vec<AgentWindowReading> = limit
            .get("unifiedWindows")
            .and_then(Value::as_object)
            .map(|windows| {
                windows
                    .iter()
                    .filter_map(|(name, window)| {
                        let used = window.get("utilization").and_then(Value::as_f64);
                        used.map(|used| AgentWindowReading {
                            window: AgentWindow::of(Some(name)),
                            // The header's utilization is a fraction; the record keeps
                            // the percentage a person reads.
                            used_percent: Some(used * 100.0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(AgentStanding {
            observed_at_ms: now_ms(),
            windows,
        })
    }

    fn take_usage(&mut self) -> Option<AgentUsage> {
        if !self.received_result {
            return None;
        }
        // Claude Code prices a conversation at list rates whether or not anything is
        // being paid, so this is always its figure and never the provider's.
        let usage = AgentUsage {
            cost_usd_micros: self.cost_usd_micros,
            cost_from_provider: false,
            is_charged: self.is_charged(),
            tokens: self.tokens,
            co2e_micrograms: 0,
        }
        .since(&self.recorded);
        (!usage.is_unused()).then_some(usage)
    }

    fn refusal(&mut self) -> Option<AgentRefusal> {
        if !self.pending.is_empty() {
            let line = mem::take(&mut self.pending);
            self.process_line(&line);
        }
        let limit = self.limit.as_ref()?;
        // A window inside `unifiedWindows` carries its own status, and a rejected
        // window there does not mean the request was refused: only the top-level
        // status says that. Anything else is a limit the account warned about and
        // then let through.
        if limit.get("status").and_then(Value::as_str) != Some("rejected") {
            return None;
        }
        let window = AgentWindow::of(limit.get("rateLimitType").and_then(Value::as_str));
        // Epoch seconds, which is what the header carries; the record keeps
        // milliseconds, like every other time it holds.
        let clears_at_ms = ["resetsAt", "overageResetsAt"]
            .iter()
            .find_map(|key| limit.get(key).and_then(Value::as_i64))
            .map(|seconds| seconds.saturating_mul(1000));
        Some(match clears_at_ms {
            // A reset that has already passed is not a clock. Holding until then
            // would put the task back into the same refusal on the first tick, so it
            // stops here and the operator hears about it once.
            Some(clears_at_ms) if clears_at_ms > now_ms() => {
                AgentRefusal::window(window, clears_at_ms)
            }
            Some(_) => AgentRefusal::stopped(format!(
                "Claude's {} limit is used up and the reset it named has already passed",
                window.word()
            )),
            None => AgentRefusal::stopped(format!(
                "Claude's {} limit is used up and it did not say when it clears",
                window.word()
            )),
        })
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

// Claude names a tool in `content_block_start` before it has any arguments, and
// repeats it complete inside the assistant message; the complete one is what says
// what the tool is aimed at, so the start only fills the gap while arguments are
// still arriving.
pub struct ClaudeProgress {
    pending: String,
    activity: Option<Activity>,
    narration: String,
    // The same buffer holds what the model is saying and what it is thinking; which of
    // the two it was decides how the line reads when the block closes.
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
                        {
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
            shell_quote(&system_prompt(AUTOPILOT_SYSTEM_PROMPT, context)),
            shell_quote(&format!(
                "{prompt}\n\nAltitude task context at the start of this turn: {context}"
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

    fn stream_decoder(&self, recorded: &TaskUsage) -> Box<dyn TaskStreamDecoder> {
        Box::new(ClaudeStream::new(recorded))
    }

    fn progress_decoder(&self) -> Option<Box<dyn TaskProgressDecoder>> {
        Some(Box::new(ClaudeProgress::new()))
    }

    fn operator_requires_conversation(&self) -> bool {
        true
    }

    fn profile_recipe(&self) -> ProfileRecipe {
        ProfileRecipe {
            profile: AgentKind::Claude.profile().to_owned(),
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
