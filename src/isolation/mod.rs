use std::{error::Error as StdError, fmt, future::Future};

use tokio::sync::watch;

mod microsandbox;

pub use microsandbox::MicrosandboxIsolation;

#[derive(Clone, Debug)]
pub enum Status {
    Starting,
    InstallingRuntime,
    PreparingBaseImage,
    Ready,
    Failed(String),
}

impl Status {
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
        status: &watch::Sender<Status>,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;
}

pub fn prepare(engine: impl IsolationEngine + 'static) -> watch::Receiver<Status> {
    let (status_tx, status_rx) = watch::channel(Status::Starting);

    tokio::spawn(async move {
        let status = match engine.prepare(&status_tx).await {
            Ok(()) => Status::Ready,
            Err(error) => Status::Failed(error.to_string()),
        };
        status_tx.send_replace(status);
    });

    status_rx
}
