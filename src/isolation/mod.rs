use std::{error::Error as StdError, fmt, future::Future, path::PathBuf, time::Duration};

use tokio::sync::{mpsc, watch};

mod microsandbox;

pub use microsandbox::MicrosandboxIsolation;

#[derive(Clone, Debug)]
pub enum IsolationStatus {
    Starting,
    InstallingRuntime,
    PreparingBaseImage,
    Ready,
    Failed(String),
}

pub struct CommandSession {
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: mpsc::UnboundedReceiver<CommandEvent>,
}

/// The agent-supplied part of preparing a reusable sandbox environment.
///
/// Isolation owns image refreshes, snapshot promotion, and cleanup. An agent
/// supplies only its identity, refresh cadence, and package-install command.
#[derive(Clone, Debug)]
pub struct AgentBaseSandboxRecipe {
    pub name: String,
    pub snapshots_dir: PathBuf,
    pub refresh_after: Duration,
    pub setup_command: String,
}

impl AgentBaseSandboxRecipe {
    pub fn new(
        name: impl Into<String>,
        snapshots_dir: PathBuf,
        refresh_after: Duration,
        setup_command: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            snapshots_dir,
            refresh_after,
            setup_command: setup_command.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentBaseSandboxSnapshot {
    pub path: PathBuf,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SandboxOptions {
    pub snapshot_path: Option<String>,
    pub image_pull_policy: ImagePullPolicy,
    pub network_enabled: bool,
    pub mounts: Vec<SandboxMount>,
    pub environment: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug)]
pub enum ImagePullPolicy {
    Never,
    Always,
}

impl SandboxOptions {
    pub fn isolated(snapshot_path: Option<String>) -> Self {
        Self {
            snapshot_path,
            image_pull_policy: ImagePullPolicy::Never,
            network_enabled: false,
            mounts: Vec::new(),
            environment: Vec::new(),
        }
    }

    /// A fresh, networked sandbox for preparing an agent environment.
    ///
    /// It always refreshes Alpine first; agent-specific setup can then update
    /// its own packages on top.
    pub fn agent_base() -> Self {
        Self {
            snapshot_path: None,
            image_pull_policy: ImagePullPolicy::Always,
            network_enabled: true,
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
    fn new(message: impl Into<String>) -> Self {
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
        recipe: AgentBaseSandboxRecipe,
    ) -> impl Future<Output = Result<AgentBaseSandboxSnapshot, IsolationError>> + Send;

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
