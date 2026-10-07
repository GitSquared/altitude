use std::{error::Error as StdError, fmt, future::Future, path::PathBuf, time::Duration};

use tokio::sync::{mpsc, watch};

use crate::development::DevelopmentEnvironment;

mod environment;
mod microsandbox;
/// Shared with the VCS providers so capsule commands are quoted one way.
pub(crate) use environment::shell_quote;
pub use environment::{DefaultFile, EnvironmentRecipe, GuestPath, PersistentDirectory};
pub use microsandbox::MicrosandboxIsolation;

#[derive(Clone, Debug)]
pub enum IsolationStatus {
    Starting,
    InstallingRuntime,
    PreparingBaseImage,
    PreparingWorkspacePackages,
    Ready,
    Failed(String),
}

pub struct CommandSession {
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: mpsc::UnboundedReceiver<CommandEvent>,
}

/// The composed recipe for a reusable sandbox environment.
///
/// Isolation owns image refreshes, snapshot promotion, and cleanup. An agent
/// contributes package installation and persistent configuration requirements.
#[derive(Clone, Debug)]
pub struct BaseSandboxRecipe {
    pub name: String,
    pub snapshots_dir: PathBuf,
    pub refresh_after: Duration,
    /// Run in order, each in its own shell. Their commands are also this
    /// recipe's identity: a snapshot is reused only while they are unchanged.
    pub steps: Vec<SetupStep>,
}

impl BaseSandboxRecipe {
    pub fn new(
        name: impl Into<String>,
        snapshots_dir: PathBuf,
        refresh_after: Duration,
        steps: Vec<SetupStep>,
    ) -> Self {
        Self {
            name: name.into(),
            snapshots_dir,
            refresh_after,
            steps,
        }
    }

    /// The work this recipe describes, as one value to compare and persist.
    pub fn setup_command(&self) -> String {
        self.steps
            .iter()
            .map(|step| step.command.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One command in an environment build, named so that an operator waiting on
/// the build can see what the wait is for.
#[derive(Clone, Debug)]
pub struct SetupStep {
    pub label: String,
    pub command: String,
}

impl SetupStep {
    pub fn new(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
        }
    }
}

/// What an environment build is doing, for the task that waits on it.
#[derive(Clone, Debug)]
pub struct EnvironmentProgress {
    /// The step in progress, in the operator's words.
    pub label: String,
    /// The exact command running, which a caller may truncate.
    pub command: String,
    /// Steps finished before this one, and the build's step count.
    pub completed: usize,
    pub total: usize,
}

/// Receives build progress. Isolation reports it; the task layer decides where
/// it goes, and a build nobody waits on can ignore it.
pub trait EnvironmentProgressSink: Send + Sync {
    fn report(&self, progress: EnvironmentProgress);
}

#[derive(Clone, Debug)]
pub struct BaseSandboxSnapshot {
    pub path: PathBuf,
    pub created_at_ms: i64,
    /// What the snapshot reported about its image and packages, empty when it
    /// could not be asked. See [`EnvironmentDescription`].
    pub alpine_release: String,
    pub packages: String,
    pub setup_command: String,
}

/// What a built environment says about itself.
///
/// Read back from the snapshot rather than derived from configuration, because
/// it records what was installed rather than what was asked for. Nothing else
/// can answer this: the image is a rolling tag and the setup commands name no
/// versions.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Read by the settings panel, which arrives with it.
pub struct EnvironmentDescription {
    /// The image's release, e.g. `3.21.3`.
    pub alpine_release: String,
    /// Installed packages as `name-version`, in the order the build listed them.
    pub packages: Vec<String>,
    pub built_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SandboxOptions {
    pub snapshot_path: Option<String>,
    pub network_enabled: bool,
    pub mounts: Vec<SandboxMount>,
    pub environment: Vec<(String, String)>,
}

impl SandboxOptions {
    pub fn isolated(snapshot_path: Option<String>) -> Self {
        Self {
            snapshot_path,
            network_enabled: false,
            mounts: Vec::new(),
            environment: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SandboxMount {
    pub host_path: PathBuf,
    pub guest_path: String,
}

pub enum Command {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Interrupt,
}

pub enum CommandEvent {
    Output {
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    Exited(i32),
    Failed(String),
}

#[derive(Clone, Copy, Debug)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub struct IsolationError(String);

impl IsolationError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for IsolationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for IsolationError {}

pub trait IsolationEngine: Send + Sync {
    fn provision(
        &self,
        id: String,
        task_snapshot: Option<String>,
        development_environment: DevelopmentEnvironment,
        recipes: Vec<EnvironmentRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn prepare(
        &self,
        status: &watch::Sender<IsolationStatus>,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn create(
        &self,
        id: String,
        options: SandboxOptions,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn ensure_base(
        &self,
        recipe: BaseSandboxRecipe,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<BaseSandboxSnapshot, IsolationError>> + Send;

    /// What a built environment says about itself, if one has been built.
    ///
    /// Synchronous because it reads a manifest rather than a machine: the answer
    /// was recorded when the snapshot was built, because only a live sandbox can
    /// say it and by the time an operator asks there is none.
    #[allow(dead_code)] // The settings panel is the caller: it describes the base environment.
    fn environment_description(&self, environment: &str) -> Option<EnvironmentDescription>;

    fn run_command(
        &self,
        id: String,
        command: String,
    ) -> impl Future<Output = Result<CommandSession, IsolationError>> + Send;

    fn remove(&self, id: String) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn suspend(
        &self,
        id: String,
        snapshot_name: String,
        snapshots_dir: PathBuf,
    ) -> impl Future<Output = Result<PathBuf, IsolationError>> + Send;

    fn remove_snapshot(
        &self,
        path: PathBuf,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn guest_regular_file_exists(
        &self,
        id: String,
        path: String,
    ) -> impl Future<Output = Result<bool, IsolationError>> + Send;
}

pub fn prepare(engine: impl IsolationEngine + 'static) -> watch::Receiver<IsolationStatus> {
    let (status_tx, status_rx) = watch::channel(IsolationStatus::Starting);

    tokio::spawn(async move {
        let status = match engine.prepare(&status_tx).await {
            Ok(()) => IsolationStatus::Ready,
            Err(error) => IsolationStatus::Failed(error.to_string()),
        };
        status_tx.send_replace(status);
    });

    status_rx
}
