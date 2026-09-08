use std::{error::Error as StdError, fmt, future::Future};

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

#[derive(Clone, Debug)]
pub struct Sandbox {
    pub id: String,
    pub status: SandboxStatus,
}

#[derive(Clone, Debug)]
pub enum SandboxStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Paused,
    Failed(String),
}

pub struct ShellSession {
    pub commands: mpsc::UnboundedSender<ShellCommand>,
    pub events: mpsc::UnboundedReceiver<ShellEvent>,
}

pub enum ShellCommand {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Close,
}

pub enum ShellEvent {
    Output(Vec<u8>),
    Exited(i32),
    Failed(String),
}

impl IsolationStatus {
    pub fn is_pending(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::InstallingRuntime | Self::PreparingBaseImage
        )
    }
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

    fn create(&self, id: String) -> impl Future<Output = Result<Sandbox, IsolationError>> + Send;

    fn statuses(
        &self,
        ids: Vec<String>,
    ) -> impl Future<Output = Result<Vec<Sandbox>, IsolationError>> + Send;

    fn open_shell(
        &self,
        id: String,
        rows: u16,
        cols: u16,
    ) -> impl Future<Output = Result<ShellSession, IsolationError>> + Send;

    fn remove(&self, id: String) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn stop_all(&self) -> impl Future<Output = Result<(), IsolationError>> + Send;
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
