use std::{fs, io::ErrorKind, os::unix::fs::symlink, path::PathBuf};

use microsandbox::{
    LocalBackend, Sandbox,
    sandbox::{PullPolicy, SandboxStatus as MicrosandboxStatus, exec::ExecEvent},
    set_default_backend,
    setup::Setup,
};
use tokio::sync::{mpsc, watch};

use crate::config::Config;

use super::{
    IsolationEngine, IsolationError, IsolationStatus, Sandbox as AltitudeSandbox, SandboxStatus,
    ShellCommand, ShellEvent, ShellSession,
};

const BASE_IMAGE: &str = "alpine";
const PREPARATION_SANDBOX: &str = "altitude-base-image-setup";

#[derive(Clone)]
pub struct MicrosandboxIsolation {
    data_home: PathBuf,
    // Microsandbox's Unix sockets exceed macOS path limits under the longer data home.
    socket_safe_home: PathBuf,
}

impl MicrosandboxIsolation {
    pub fn new(config: &Config) -> Self {
        Self {
            data_home: config.isolation_data_dir(),
            socket_safe_home: config.microsandbox_socket_dir(),
        }
    }

    fn ensure_socket_safe_home(&self) -> Result<PathBuf, IsolationError> {
        let parent = self
            .socket_safe_home
            .parent()
            .ok_or_else(|| IsolationError::new("altitude's cache directory has no parent"))?;
        fs::create_dir_all(parent).map_err(|error| IsolationError::new(error.to_string()))?;

        match fs::read_link(&self.socket_safe_home) {
            Ok(target) if target == self.data_home => Ok(self.socket_safe_home.clone()),
            Ok(target) => Err(IsolationError::new(format!(
                "{} points to {}, expected {}",
                self.socket_safe_home.display(),
                target.display(),
                self.data_home.display()
            ))),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                symlink(&self.data_home, &self.socket_safe_home)
                    .map_err(|error| IsolationError::new(error.to_string()))?;
                Ok(self.socket_safe_home.clone())
            }
            Err(error) => Err(IsolationError::new(error.to_string())),
        }
    }
}

impl IsolationEngine for MicrosandboxIsolation {
    async fn prepare(&self, status: &watch::Sender<IsolationStatus>) -> Result<(), IsolationError> {
        status.send_replace(IsolationStatus::InstallingRuntime);
        Setup::builder()
            .base_dir(&self.data_home)
            .build()
            .install()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;

        let socket_safe_home = self.ensure_socket_safe_home()?;
        let backend = LocalBackend::builder()
            .home(socket_safe_home)
            .build()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        set_default_backend(backend);

        status.send_replace(IsolationStatus::PreparingBaseImage);
        let sandbox = Sandbox::builder(PREPARATION_SANDBOX)
            .image(BASE_IMAGE)
            .pull_policy(PullPolicy::IfMissing)
            .disable_network()
            .ephemeral(true)
            .replace()
            .create()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .stop()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;

        Ok(())
    }

    async fn create(&self, id: String) -> Result<AltitudeSandbox, IsolationError> {
        Sandbox::builder(&id)
            .image(BASE_IMAGE)
            .pull_policy(PullPolicy::Never)
            .disable_network()
            .create()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;

        Ok(AltitudeSandbox {
            id,
            status: SandboxStatus::Running,
        })
    }

    async fn statuses(&self, ids: Vec<String>) -> Result<Vec<AltitudeSandbox>, IsolationError> {
        let mut sandboxes = Vec::with_capacity(ids.len());

        for id in ids {
            let status = Sandbox::get(&id)
                .await
                .map_err(|error| IsolationError::new(error.to_string()))?
                .status_snapshot();
            sandboxes.push(AltitudeSandbox {
                id,
                status: match status {
                    MicrosandboxStatus::Created | MicrosandboxStatus::Stopped => {
                        SandboxStatus::Stopped
                    }
                    MicrosandboxStatus::Starting => SandboxStatus::Starting,
                    MicrosandboxStatus::Running => SandboxStatus::Running,
                    MicrosandboxStatus::Draining => SandboxStatus::Stopping,
                    MicrosandboxStatus::Paused => SandboxStatus::Paused,
                    MicrosandboxStatus::Crashed => SandboxStatus::Failed("sandbox crashed".into()),
                },
            });
        }

        Ok(sandboxes)
    }

    async fn open_shell(
        &self,
        id: String,
        rows: u16,
        cols: u16,
    ) -> Result<ShellSession, IsolationError> {
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?
            .connect_or_start_detached()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let mut process = sandbox
            .exec_stream_with("sh", |exec| {
                exec.arg("-i")
                    .env("TERM", "xterm-256color")
                    .stdin_pipe()
                    .tty(true)
            })
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        process
            .resize(rows, cols)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let stdin = process
            .take_stdin()
            .ok_or_else(|| IsolationError::new("shell stdin was not available"))?;
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let _sandbox = sandbox;
            loop {
                tokio::select! {
                    command = command_rx.recv() => match command {
                        Some(ShellCommand::Input(bytes)) => {
                            if let Err(error) = stdin.write(bytes).await {
                                let _ = event_tx.send(ShellEvent::Failed(error.to_string()));
                                break;
                            }
                        }
                        Some(ShellCommand::Resize { rows, cols }) => {
                            let _ = process.resize(rows, cols).await;
                        }
                        Some(ShellCommand::Close) | None => {
                            let _ = process.kill().await;
                            break;
                        }
                    },
                    event = process.recv() => match event {
                        Some(ExecEvent::Stdout(bytes) | ExecEvent::Stderr(bytes)) => {
                            let _ = event_tx.send(ShellEvent::Output(bytes.to_vec()));
                        }
                        Some(ExecEvent::Exited { code }) => {
                            let _ = event_tx.send(ShellEvent::Exited(code));
                            break;
                        }
                        None => {
                            let _ = event_tx.send(ShellEvent::Failed(
                                "shell connection closed without an exit status".into(),
                            ));
                            break;
                        }
                        Some(ExecEvent::Failed(error)) => {
                            let _ = event_tx.send(ShellEvent::Failed(error.message));
                            break;
                        }
                        Some(ExecEvent::Started { .. }) => {}
                        Some(ExecEvent::StdinError(error)) => {
                            let _ = event_tx.send(ShellEvent::Failed(format!("shell stdin failed: {error:?}")));
                        }
                    }
                }
            }
        });

        Ok(ShellSession {
            commands: command_tx,
            events: event_rx,
        })
    }

    async fn remove(&self, id: String) -> Result<(), IsolationError> {
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .stop()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .remove()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    async fn stop_all(&self) -> Result<(), IsolationError> {
        let mut cursor = None;
        let mut first_error = None;

        loop {
            let page = Sandbox::list_with(|list| match &cursor {
                Some(cursor) => list.limit(100).cursor(cursor),
                None => list.limit(100),
            })
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;

            for sandbox in page
                .sandboxes
                .into_iter()
                .filter(|sandbox| sandbox.name().starts_with("altitude-"))
            {
                if sandbox.status_snapshot().has_active_runtime_state()
                    && let Err(error) = sandbox.stop().await
                    && first_error.is_none()
                {
                    first_error = Some(IsolationError::new(error.to_string()));
                }
                if let Err(error) = sandbox.remove().await
                    && first_error.is_none()
                {
                    first_error = Some(IsolationError::new(error.to_string()));
                }
            }

            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }

        first_error.map_or(Ok(()), Err)
    }
}
