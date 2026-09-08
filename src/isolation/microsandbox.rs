use std::{fs, io::ErrorKind, os::unix::fs::symlink, path::PathBuf};

use directories::ProjectDirs;
use microsandbox::{LocalBackend, Sandbox, sandbox::PullPolicy, set_default_backend, setup::Setup};
use tokio::sync::watch;

use super::{IsolationEngine, IsolationError, Status};

const BASE_IMAGE: &str = "alpine";
const PREPARATION_SANDBOX: &str = "altitude-base-image-setup";

pub struct MicrosandboxIsolation {
    data_home: PathBuf,
    // Microsandbox's Unix sockets exceed macOS path limits under the longer data home.
    socket_safe_home: PathBuf,
}

impl MicrosandboxIsolation {
    pub fn new() -> Result<Self, IsolationError> {
        let project_dirs = ProjectDirs::from("", "", "altitude")
            .ok_or_else(|| IsolationError::new("could not determine altitude's data directory"))?;

        Ok(Self {
            data_home: project_dirs.data_dir().join("isolation"),
            socket_safe_home: project_dirs.cache_dir().join("msb"),
        })
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
    async fn prepare(&self, status: &watch::Sender<Status>) -> Result<(), IsolationError> {
        status.send_replace(Status::InstallingRuntime);
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

        status.send_replace(Status::PreparingBaseImage);
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
}
