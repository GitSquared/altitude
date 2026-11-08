// Helpers for driving Altitude without a real agent, network or microVM. Compiled
// under `cfg(test)` for the module tests, and behind the `testkit` feature for a
// driver that wants the same scripted harness in a real binary.
#![allow(dead_code)]

use std::{path::Path, sync::Arc};

use crate::{
    agent::{AgentEngine, AgentEngines, AgentKind, AgentUsage, TaskStreamDecoder, TaskUsage},
    config::Config,
    isolation::{DefaultFile, GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand},
    project::Project,
    task::TaskAgentTurnContext,
    vcs::VcsProviderKind,
};

// Where the scripted agent keeps its profile in the sandbox. It is a directory the
// plan mounts, so the transcript a turn writes lands on the machine, exactly as a
// real harness's would.
const SCRIPTED_DIR: &str = "/altitude/scripted";

// What a scripted turn emits, in the shape a real harness's stream is decoded from.
// One line per fact, so a chunk boundary can fall anywhere.
pub const SCRIPTED_STREAM: &str = concat!(
    "{\"type\":\"activity\",\"text\":\"reading the task\"}\n",
    "{\"type\":\"model\",\"model\":\"scripted-1\"}\n",
    "{\"type\":\"usage\",\"cost_usd_micros\":1200,\"cost_from_provider\":false,\"is_charged\":true}\n",
    "{\"type\":\"result\",\"summary\":\"scripted turn complete\"}\n",
);

// A configuration pointed at directories under one root, so a test's state is its
// own and nothing global has to be set.
pub fn config(root: &Path) -> Config {
    Config::at(root.join("config"), root.join("data"), root.join("cache"))
}

// A project that runs a scripted agent and names no repository: enough to reach the
// turn loop with nothing to clone and nothing to install.
pub fn project(name: &str) -> Project {
    let mut project = Project::new(name);
    project.agent = Some(AgentKind::Pi);
    project
}

// A stand-in for a version-control provider: no install, no mounts, no network.
pub fn scripted_vcs_recipe() -> ProfileRecipe {
    ProfileRecipe {
        profile: VcsProviderKind::GitHub.profile().to_owned(),
        display: "Scripted VCS",
        sign_in: "Nothing to sign in.",
        sign_in_command: SignInCommand::Direct("true"),
        status_command: "true",
        label: "the scripted version control",
        setup_command: "",
        directories: Vec::new(),
        environment: Vec::new(),
    }
}

// A harness that runs one scripted turn: it writes the stream to its transcript and
// prints it, which is what a real harness does in the two places the turn loop and
// the transcript transfer each look.
pub struct ScriptedAgent;

impl AgentEngine for ScriptedAgent {
    fn profile_recipe(&self) -> ProfileRecipe {
        ProfileRecipe {
            profile: AgentKind::Pi.profile().to_owned(),
            display: "Scripted",
            sign_in: "Nothing to sign in.",
            sign_in_command: SignInCommand::Direct("true"),
            status_command: "true",
            label: "the scripted agent",
            setup_command: "",
            directories: vec![PersistentDirectory::root(
                GuestPath::Absolute(SCRIPTED_DIR),
                "The scripted agent's profile.",
                Vec::<DefaultFile>::new(),
            )],
            environment: Vec::new(),
        }
    }

    fn new_conversation_id(&self) -> String {
        "scripted-conversation".to_owned()
    }

    fn conversation_transcript_path(&self, conversation_id: &str) -> String {
        format!("{SCRIPTED_DIR}/sessions/{conversation_id}.jsonl")
    }

    fn autopilot_command(
        &self,
        conversation_id: &str,
        _prompt: &str,
        _context: &TaskAgentTurnContext,
        _resume: bool,
    ) -> String {
        let transcript = self.conversation_transcript_path(conversation_id);
        format!(
            "set -eu\nmkdir -p {SCRIPTED_DIR}/sessions\ncat > {transcript} <<'SCRIPTED_TURN'\n{SCRIPTED_STREAM}SCRIPTED_TURN\ncat {transcript}\n"
        )
    }

    fn operator_command(&self, _conversation_id: &str, _opening: &str) -> String {
        "exec bash -l".to_owned()
    }

    fn operator_requires_conversation(&self) -> bool {
        false
    }

    fn stream_decoder(&self, _recorded: &TaskUsage) -> Box<dyn TaskStreamDecoder> {
        Box::new(ScriptedStream::default())
    }
}

// The decoder for `SCRIPTED_STREAM`, buffering by line the way a harness that
// streams JSON does.
#[derive(Default)]
pub struct ScriptedStream {
    pending: String,
    model: Option<String>,
    usage: Option<AgentUsage>,
    failure: Option<String>,
}

impl ScriptedStream {
    fn line(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            return;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("model") => {
                self.model = value
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
            }
            Some("usage") => {
                self.usage = Some(AgentUsage {
                    cost_usd_micros: value
                        .get("cost_usd_micros")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or_default(),
                    cost_from_provider: value
                        .get("cost_from_provider")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or_default(),
                    is_charged: value
                        .get("is_charged")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or_default(),
                });
            }
            Some("failure") => {
                self.failure = value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
}

impl TaskStreamDecoder for ScriptedStream {
    fn process(&mut self, bytes: &[u8]) {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        while let Some(index) = self.pending.find('\n') {
            let line = self.pending[..index].to_owned();
            self.pending.drain(..=index);
            self.line(&line);
        }
    }

    fn failure_message(&mut self) -> Option<&str> {
        self.failure.as_deref()
    }

    fn take_model(&mut self) -> Option<String> {
        self.model.take()
    }

    fn take_usage(&mut self) -> Option<AgentUsage> {
        self.usage.take()
    }
}

pub fn engines() -> AgentEngines {
    AgentEngines::new().with(AgentKind::Pi, Arc::new(ScriptedAgent))
}
