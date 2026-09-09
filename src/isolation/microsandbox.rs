use std::{fs, io::ErrorKind, os::unix::fs::symlink, path::PathBuf, time::Duration};

use microsandbox::{
    LocalBackend, Sandbox,
    sandbox::{PullPolicy, exec::ExecEvent},
    set_default_backend,
    setup::Setup,
    snapshot::Snapshot,
};
use tokio::sync::{mpsc, watch};

use crate::config::Config;

use super::{
    Command, CommandEvent, CommandSession, IsolationEngine, IsolationError, IsolationStatus,
    OutputStream,
};

const BASE_IMAGE: &str = "alpine";
const PREPARATION_SANDBOX: &str = "altitude-base-image-setup";

#[derive(Clone)]
pub struct MicrosandboxIsolation {
    data_home: PathBuf,
    cache_home: PathBuf,
    // Microsandbox's Unix sockets exceed macOS path limits under the longer data home.
    socket_safe_home: PathBuf,
}

impl MicrosandboxIsolation {
    pub fn new(config: &Config) -> Self {
        Self {
            data_home: config.isolation_data_dir(),
            cache_home: config.isolation_cache_dir(),
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
            .cache_dir(&self.cache_home)
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

    async fn create(
        &self,
        id: String,
        snapshot_path: Option<String>,
    ) -> Result<(), IsolationError> {
        let builder = Sandbox::builder(&id).disable_network();
        match snapshot_path {
            Some(snapshot_path) => builder.from_snapshot(snapshot_path).create_detached().await,
            None => {
                builder
                    .image(BASE_IMAGE)
                    .pull_policy(PullPolicy::Never)
                    .create_detached()
                    .await
            }
        }
        .map_err(|error| IsolationError::new(error.to_string()))?;

        Ok(())
    }

    async fn run_command(
        &self,
        id: String,
        command: String,
    ) -> Result<CommandSession, IsolationError> {
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?
            .connect_or_start()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let mut process = sandbox
            .shell_stream_with(command, |exec| exec.stdin_pipe().tty(true))
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let stdin = process
            .take_stdin()
            .ok_or_else(|| IsolationError::new("task stdin was not available"))?;
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let _sandbox = sandbox;
            loop {
                tokio::select! {
                    command = command_rx.recv() => match command {
                        Some(Command::Input(bytes)) => {
                            if let Err(error) = stdin.write(bytes).await {
                                let _ = event_tx.send(CommandEvent::Failed(error.to_string()));
                                break;
                            }
                        }
                        Some(Command::Resize { rows, cols }) => {
                            let _ = process.resize(rows, cols).await;
                        }
                        None => break,
                    },
                    event = process.recv() => {
                        let event = match event {
                            Some(ExecEvent::Stdout(bytes)) => CommandEvent::Output {
                                stream: OutputStream::Stdout,
                                bytes: bytes.to_vec(),
                            },
                            Some(ExecEvent::Stderr(bytes)) => CommandEvent::Output {
                                stream: OutputStream::Stderr,
                                bytes: bytes.to_vec(),
                            },
                            Some(ExecEvent::Exited { code }) => CommandEvent::Exited(code),
                            Some(ExecEvent::Failed(error)) => CommandEvent::Failed(error.message),
                            Some(ExecEvent::Started { .. } | ExecEvent::StdinError(_)) => continue,
                            None => CommandEvent::Failed(
                                "command connection closed without an exit status".into(),
                            ),
                        };
                        let terminal = !matches!(event, CommandEvent::Output { .. });
                        let _ = event_tx.send(event);
                        if terminal {
                            break;
                        }
                    }
                }
            }
        });

        Ok(CommandSession {
            commands: command_tx,
            events: event_rx,
        })
    }

    async fn remove(&self, id: String) -> Result<(), IsolationError> {
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .stop_with_timeout(Duration::from_secs(10))
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .remove()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    async fn suspend(
        &self,
        id: String,
        snapshot_name: String,
        snapshots_dir: PathBuf,
    ) -> Result<PathBuf, IsolationError> {
        fs::create_dir_all(&snapshots_dir)
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .stop_with_timeout(Duration::from_secs(10))
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let snapshot = Snapshot::builder(snapshot_name)
            .from_sandbox(&id)
            .dest_dir(snapshots_dir)
            .force()
            .create()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .remove()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        Ok(snapshot.path().to_owned())
    }

    async fn remove_snapshot(&self, path: PathBuf) -> Result<(), IsolationError> {
        Snapshot::remove(&path.to_string_lossy(), false)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }
}
