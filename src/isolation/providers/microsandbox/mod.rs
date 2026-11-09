use std::{
    fs,
    io::{self, ErrorKind},
    os::unix::fs::symlink,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use microsandbox::{
    LocalBackend, Sandbox,
    setup::{
        InstallOptions, RuntimeOrigin, Version, ensure_runtime, install_runtime,
        resolve_runtime_version,
    },
};
use tokio::{process::Command as TokioCommand, task::spawn_blocking};

use crate::agent::mcp::AGENT_CHANNEL_PORT;
use crate::config::Config;

use crate::isolation::{CREDENTIAL_PREFIX, IsolationError, VsockRoute, capacity::HostPressure};

mod base;
mod engine;
mod files;
mod lifecycle;
mod packages;
mod snapshot;
mod transfer;

pub(super) async fn connect(id: &str) -> Result<Sandbox, IsolationError> {
    Sandbox::get(id)
        .await
        .map_err(|error| IsolationError::new(error.to_string()))?
        .connect_or_start()
        .await
        .map_err(|error| IsolationError::new(error.to_string()))
}

#[derive(Clone)]
pub struct MicrosandboxIsolation {
    config: Config,
    data_home: PathBuf,
    cache_home: PathBuf,
    socket_safe_home: PathBuf,
    build_lock: BuildLock,
    // Shared rather than per view: how much this machine can take is the same
    // answer for every operator on it.
    host_pressure: Arc<Mutex<HostPressure>>,
}

#[derive(Clone)]
pub(super) struct BuildLock {
    path: PathBuf,
}

pub(super) struct BuildGuard {
    _file: fs::File,
}

pub(super) fn output_tail(output: &str) -> &str {
    const LIMIT: usize = 1_000;
    let mut start = output.len().saturating_sub(LIMIT);
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    output[start..].trim()
}

impl MicrosandboxIsolation {
    pub(super) async fn ensure_compatible_runtime(
        backend: &LocalBackend,
    ) -> Result<(), IsolationError> {
        let mut options = InstallOptions::default();
        let expected = Version::parse(&options.version)
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let runtime = ensure_runtime(backend.config(), options.clone())
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let installed = match resolve_runtime_version(&runtime.msb_path)
            .map_err(|error| IsolationError::new(error.to_string()))?
        {
            Some(version) => version,
            None => {
                let output = TokioCommand::new(&runtime.msb_path)
                    .arg("--version")
                    .output()
                    .await
                    .map_err(|error| IsolationError::new(error.to_string()))?;
                let version = String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .next_back()
                    .ok_or_else(|| IsolationError::new("Microsandbox returned no version"))?
                    .trim_start_matches('v')
                    .to_owned();
                Version::parse(&version).map_err(|error| IsolationError::new(error.to_string()))?
            }
        };
        if installed == expected {
            return Ok(());
        }
        if runtime.origin != RuntimeOrigin::Home {
            return Err(IsolationError::new(format!(
                "Microsandbox runtime {installed} at {} does not match SDK {expected}; update the configured runtime",
                runtime.msb_path.display()
            )));
        }

        tracing::info!(%installed, %expected, "updating Microsandbox runtime");
        options.force = true;
        install_runtime(backend.config(), options)
            .await
            .map(|_| ())
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    pub(crate) fn check_host() -> Result<(), IsolationError> {
        #[cfg(target_os = "linux")]
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map_err(|error| {
                let guidance = match error.kind() {
                    ErrorKind::PermissionDenied => "Grant the account running Altitude read/write access to /dev/kvm. On Ubuntu, run `sudo usermod -aG kvm $(id -un)`, then reconnect your SSH session (or log out and back in) and restart Altitude. For a service, grant access to its service account instead.",
                    ErrorKind::NotFound => "Enable hardware virtualization in the host firmware and load the appropriate KVM kernel module. If this machine is itself a VM, its host must expose nested virtualization and /dev/kvm.",
                    _ => "Check that hardware virtualization is available and /dev/kvm is accessible to the account running Altitude.",
                };
                IsolationError::new(format!(
                    "Microsandbox requires Linux KVM, but cannot open /dev/kvm: {error}.\n{guidance}"
                ))
            })?;
        Ok(())
    }

    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            data_home: config.isolation_data_dir(),
            cache_home: config.isolation_cache_dir(),
            socket_safe_home: config.cache_dir().join("msb"),
            build_lock: BuildLock::new(config),
            host_pressure: Arc::new(Mutex::new(HostPressure::new(&config.isolation_data_dir()))),
        }
    }

    pub(super) fn credential_shell_channel(&self, id: &str) -> Option<VsockRoute> {
        if !id.starts_with(&format!("{CREDENTIAL_PREFIX}shell-")) {
            return None;
        }
        let host_socket = self.config.agent_channel_dir().join(format!("{id}.sock"));
        host_socket.exists().then_some(VsockRoute {
            host_socket,
            guest_port: AGENT_CHANNEL_PORT,
        })
    }

    pub(super) fn ensure_socket_safe_home(&self) -> Result<PathBuf, IsolationError> {
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

impl BuildLock {
    pub(super) fn new(config: &Config) -> Self {
        Self {
            path: config.data_dir().join("environments").join("build.lock"),
        }
    }

    pub(super) async fn acquire(&self) -> Result<BuildGuard, IsolationError> {
        let path = self.path.clone();
        spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)?;
            file.lock()?;
            Ok(BuildGuard { _file: file })
        })
        .await
        .map_err(|error| IsolationError::new(error.to_string()))?
        .map_err(|error: io::Error| IsolationError::new(error.to_string()))
    }
}
