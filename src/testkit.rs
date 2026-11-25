// Helpers for driving Altitude without a real agent, network or microVM. Compiled
// under `cfg(test)`, in a debug build, and behind the `testkit` feature: the module
// tests use it, and a debug build honours `ALTITUDE_SCRIPTED_AGENT` so the interface
// can be driven end to end without an account, a network or a model.
#![allow(dead_code)]

use std::{env, fs, io, path::Path, sync::Arc};

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
        session_file: None,
        session_deadline: None,
        session_renew: None,
        session_keep: &[],
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
            session_file: None,
            session_deadline: None,
            session_renew: None,
            session_keep: &[],
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

// Whether this run was asked for a scripted harness. A debug build, or one with the
// `testkit` feature, honours it; a release build does not know the name.
pub fn is_scripted() -> bool {
    env::var_os("ALTITUDE_SCRIPTED_AGENT").is_some()
}

// A controlled credential renewal: a short-lived session, a harness that writes a
// later one, and the keeper. It proves the schedule the real harnesses are on
// without an account, a network or a model. The verdict is the exit code; the log
// lines are the evidence.
pub async fn credential_renewal_fixture() -> Result<(), String> {
    use crate::{
        credentials::{Credentials, freshness, keeper::Keeper},
        isolation::{PortRelays, TestbedIsolation},
    };
    use tokio::time::Instant;

    const PROFILE: &str = "claude";
    const SESSION: &str = "fixture-session.json";
    const ATTEMPTS: &str = "fixture-attempts";
    // Far enough ahead that a renewal is unmistakable, and fixed so the fixture needs
    // no clock inside the sandbox.
    const RENEWED_AT_MS: i64 = 4_102_444_800_000;
    const RENEW_COMMAND: &str = r#"printf '%s' '{"expiresAt":4102444800000}' > /altitude/fixture/fixture-session.json; echo x >> /altitude/fixture/fixture-attempts"#;
    const REFUSE_COMMAND: &str = r#"echo x >> /altitude/fixture/fixture-attempts; exit 1"#;
    const UNCHANGED_COMMAND: &str = r#"echo x >> /altitude/fixture/fixture-attempts"#;

    // The keeper's own lines are the evidence that only profile, deadline and attempt
    // are logged, so a fixture run shows them.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .try_init();

    fn deadline(contents: &str) -> Option<i64> {
        let value: serde_json::Value = serde_json::from_str(contents).ok()?;
        value.get("expiresAt")?.as_i64()
    }

    fn recipe(renew: &'static str) -> ProfileRecipe {
        ProfileRecipe {
            profile: PROFILE.to_owned(),
            display: "Fixture harness",
            sign_in: "",
            sign_in_command: SignInCommand::Direct("true"),
            status_command: "true",
            session_file: Some(SESSION),
            session_deadline: Some(deadline),
            session_renew: Some(renew),
            session_keep: &[SESSION],
            label: "the fixture harness",
            setup_command: "",
            directories: vec![PersistentDirectory::root(
                GuestPath::Absolute("/altitude/fixture"),
                "",
                Vec::<DefaultFile>::new(),
            )],
            environment: Vec::new(),
        }
    }

    let root = tempfile::tempdir().map_err(|error| error.to_string())?;
    let config = Config::at(
        root.path().join("config"),
        root.path().join("data"),
        root.path().join("cache"),
    );
    let profile = config.profile_dir(PROFILE);
    fs::create_dir_all(&profile).map_err(|error| error.to_string())?;
    let session = profile.join(SESSION);
    let attempts = profile.join(ATTEMPTS);

    let mut credentials = Credentials::new(
        TestbedIsolation::new(&config),
        config.clone(),
        AgentEngines::new(),
        PortRelays::new(config.agent_channel_dir()),
    );
    credentials.override_recipe(PROFILE, recipe(RENEW_COMMAND));

    let due = freshness::now_ms() + 60_000;
    write_session(&session, due)?;

    // Idle renewal: one pass of a keeper renews a session before it runs out.
    let mut keeper = Keeper::new(credentials.clone());
    let _ = keeper.poll().await;
    let renewed = read_deadline(&session)?;
    if renewed != RENEWED_AT_MS {
        return Err(format!(
            "an idle keeper did not renew the session: {renewed:?}"
        ));
    }
    println!("idle renewal: {} -> {renewed}", due);

    // Restart recovery: a keeper built after a restart carries nothing across; its
    // first pass reads the deadline from the file.
    write_session(&session, due)?;
    drop(keeper);
    let mut restarted = Keeper::new(credentials.clone());
    let _ = restarted.poll().await;
    let recovered = read_deadline(&session)?;
    if recovered != RENEWED_AT_MS {
        return Err(format!("a restarted keeper did not renew: {recovered:?}"));
    }
    println!("restart recovery: {} -> {recovered}", due);

    // Single flight: two callers at once make one sandbox. The first renews, the
    // second waits and finds the deadline already moved.
    write_session(&session, due)?;
    let _ = fs::remove_file(&attempts);
    let (first, second) = tokio::join!(
        credentials.renew_if_due(PROFILE),
        credentials.renew_if_due(PROFILE),
    );
    first.map_err(|error| error.to_string())?;
    second.map_err(|error| error.to_string())?;
    let made = attempts_made(&attempts)?;
    if made != 1 {
        return Err(format!("two concurrent renewals made {made} attempts"));
    }
    println!("single flight: two callers, {made} attempt");

    // A refusal is visible and is not retried on the next pass: the second poll
    // finds the profile in backoff and leaves it alone.
    credentials.override_recipe(PROFILE, recipe(REFUSE_COMMAND));
    write_session(&session, due)?;
    let _ = fs::remove_file(&attempts);
    let mut keeper = Keeper::new(credentials.clone());
    let _ = keeper.poll().await;
    let _ = keeper.poll().await;
    let made = attempts_made(&attempts)?;
    if made != 1 {
        return Err(format!(
            "a refused renewal was retried {made} times without waiting"
        ));
    }
    println!("refusal: {made} attempt, then backoff");

    // A harness that is not ready to refresh yet leaves the deadline alone: the keeper
    // asks again just before the session runs out, not on a doubling clock that can
    // carry the retry past it.
    credentials.override_recipe(PROFILE, recipe(UNCHANGED_COMMAND));
    let expires = freshness::now_ms() + 10 * 60_000;
    write_session(&session, expires)?;
    let _ = fs::remove_file(&attempts);
    let mut keeper = Keeper::new(credentials.clone());
    let wake = keeper.poll().await;
    let made = attempts_made(&attempts)?;
    let retry_in_s = wake.saturating_duration_since(Instant::now()).as_secs();
    if made != 1 || !(7 * 60..=8 * 60).contains(&retry_in_s) {
        return Err(format!(
            "an unchanged renewal made {made} attempts and retries in {retry_in_s}s"
        ));
    }
    println!("unchanged: {made} attempt, retry in {retry_in_s}s, before the deadline");

    println!("credential renewal fixture passed");
    Ok(())
}

fn write_session(path: &Path, expires_at_ms: i64) -> Result<(), String> {
    fs::write(path, format!("{{\"expiresAt\":{expires_at_ms}}}")).map_err(|error| error.to_string())
}

fn read_deadline(path: &Path) -> Result<i64, String> {
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&contents).map_err(|error| error.to_string())?;
    value
        .get("expiresAt")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| format!("no deadline in {contents}"))
}

fn attempts_made(path: &Path) -> Result<usize, String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents.lines().count()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.to_string()),
    }
}
