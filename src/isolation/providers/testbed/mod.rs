use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;

use crate::{
    config::Config,
    isolation::{
        BaseSandboxRecipe, BaseSandboxSnapshot, EnvironmentProgress, EnvironmentProgressSink,
        IsolationError, SetupStep, capacity::HostPressure, private_directory,
    },
};

mod engine;
mod files;
mod machine;

// The marker a built layer leaves in its snapshot directory. It is not a snapshot:
// the machine is the base, and the marker only records that the layer's commands
// have already run here. The directory is named after the commands, so a changed
// command builds again without anything having to compare them.
const LAYER_MARKER: &str = "testbed-built";

// The directories every sandbox is given over the machine rather than from its own
// disk. A command resolves paths against these, so the same absolute path means the
// machine's file here and now.
const SYSTEM_ROOTS: [&str; 7] = ["/root", "/usr", "/etc", "/dev", "/sys", "/proc", "/run"];

// A weaker isolation primitive for running Altitude inside one of its own sandboxes,
// where a nested microVM has no `/dev/kvm` to boot against. Each sandbox is a
// directory used as a `chroot` root, and every command runs in a private mount
// namespace that binds the machine's system trees and the plan's mounts over it.
// It is not a boundary: a testbed task shares the machine's `/usr` and `/root`, and
// can write them. That is the point — the machine is already a disposable sandbox.
#[derive(Clone)]
pub struct TestbedIsolation {
    config: Config,
    // Shared rather than per view, like the microVM provider's: how much this
    // machine can take is the same answer for every operator on it.
    host_pressure: Arc<Mutex<HostPressure>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct MountRecord {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub readonly: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct SandboxRecord {
    pub mounts: Vec<MountRecord>,
    pub environment: Vec<(String, String)>,
}

impl TestbedIsolation {
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            host_pressure: Arc::new(Mutex::new(HostPressure::new(&config.isolation_data_dir()))),
        }
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.config.isolation_data_dir().join("testbed")
    }

    fn sandbox_dir(&self, id: &str) -> PathBuf {
        self.sandboxes_dir().join(id)
    }

    pub(super) fn sandbox_root(&self, id: &str) -> PathBuf {
        self.sandbox_dir(id).join("root")
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.sandbox_dir(id).join("sandbox.json")
    }

    fn write_record(&self, id: &str, record: &SandboxRecord) -> Result<(), IsolationError> {
        let encoded =
            serde_json::to_vec(record).map_err(|error| IsolationError::new(error.to_string()))?;
        fs::write(self.record_path(id), encoded)
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    pub(super) fn read_record(&self, id: &str) -> Result<SandboxRecord, IsolationError> {
        let path = self.record_path(id);
        let text = fs::read(&path).map_err(|error| {
            IsolationError::missing_sandbox(format!(
                "this testbed sandbox has no record at {}: {error}",
                path.display()
            ))
        })?;
        serde_json::from_slice(&text).map_err(|error| IsolationError::new(error.to_string()))
    }

    pub(super) fn exists(&self, id: &str) -> bool {
        self.sandbox_root(id).is_dir()
    }

    // Where a guest path is on the machine. A mount wins if it names the path or a
    // parent of it; the system trees are the machine's own; anything else is inside
    // the sandbox's directory.
    pub(super) fn host_path(&self, id: &str, record: &SandboxRecord, guest: &str) -> PathBuf {
        let guest = guest.trim_end_matches('/');
        if let Some(mount) = record
            .mounts
            .iter()
            .filter(|mount| names(guest, &mount.guest_path))
            .max_by_key(|mount| mount.guest_path.trim_end_matches('/').len())
        {
            let base = mount.guest_path.trim_end_matches('/');
            return match guest.strip_prefix(base) {
                None | Some("") => mount.host_path.clone(),
                Some(suffix) => mount.host_path.join(suffix.trim_start_matches('/')),
            };
        }
        for root in SYSTEM_ROOTS {
            if guest == root {
                return PathBuf::from(root);
            }
            if let Some(suffix) = guest.strip_prefix(&format!("{root}/")) {
                return Path::new(root).join(suffix);
            }
        }
        self.sandbox_root(id).join(guest.trim_start_matches('/'))
    }

    fn host_base_snapshot(&self) -> BaseSandboxSnapshot {
        BaseSandboxSnapshot {
            path: self.sandboxes_dir().join("base"),
            created_at_ms: now_ms(),
            os_release: os_release(),
            packages: String::new(),
            setup_command: String::new(),
        }
    }

    // The machine is the base, so a layer is "built" once its commands have run
    // here. The marker is written only after they have, so a build killed part-way
    // runs again.
    async fn ensure_layer(
        &self,
        layer: BaseSandboxRecipe,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        let marker = layer.snapshots_dir.join(LAYER_MARKER);
        if marker.is_file() {
            return Ok(BaseSandboxSnapshot {
                path: marker,
                created_at_ms: now_ms(),
                os_release: os_release(),
                packages: String::new(),
                setup_command: layer.setup_command(),
            });
        }
        private_directory(&layer.snapshots_dir)?;
        let steps: Vec<&SetupStep> = layer
            .steps
            .iter()
            .filter(|step| !step.command.trim().is_empty())
            .collect();
        for (completed, step) in steps.iter().enumerate() {
            progress.report(EnvironmentProgress {
                label: step.label.clone(),
                completed,
                total: steps.len(),
            });
            self.run_on_host(&step.command)
                .await
                .map_err(|error| IsolationError::new(format!("{}: {error}", step.label)))?;
        }
        fs::write(&marker, b"built\n").map_err(|error| IsolationError::new(error.to_string()))?;
        Ok(BaseSandboxSnapshot {
            path: marker,
            created_at_ms: now_ms(),
            os_release: os_release(),
            packages: String::new(),
            setup_command: layer.setup_command(),
        })
    }

    // A layer's commands are the machine's own setup, run where the sandbox can see
    // their result: on the machine, not in a chroot. The toolchains a project
    // installs live in `/usr` and `/root`, which every sandbox binds.
    async fn run_on_host(&self, command: &str) -> Result<(), IsolationError> {
        let output = TokioCommand::new("/bin/bash")
            .arg("-c")
            .arg(command)
            .env("HOME", "/root")
            .env("PATH", machine_path())
            .output()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = output_tail(&String::from_utf8_lossy(&output.stderr)).to_owned();
        let said = if stderr.is_empty() {
            output_tail(&String::from_utf8_lossy(&output.stdout)).to_owned()
        } else {
            stderr
        };
        Err(IsolationError::new(format!(
            "setup exited with code {:?}: {said}",
            output.status.code()
        )))
    }
}

fn names(path: &str, mount: &str) -> bool {
    let mount = mount.trim_end_matches('/');
    path == mount || path.starts_with(&format!("{mount}/"))
}

pub(super) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(super) fn os_release() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|value| value.trim_matches('"').to_owned())
        })
        .unwrap_or_default()
}

pub(super) fn machine_path() -> String {
    "/root/.cargo/bin:/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        .to_owned()
}

pub(super) fn output_tail(output: &str) -> &str {
    const LIMIT: usize = 1_000;
    let mut start = output.len().saturating_sub(LIMIT);
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    output[start..].trim()
}

pub(super) fn check_environment() -> Result<(), IsolationError> {
    for binary in ["/usr/bin/unshare", "/usr/sbin/chroot", "/usr/bin/mount"] {
        if !Path::new(binary).is_file() {
            return Err(IsolationError::new(format!(
                "the testbed isolation needs {binary}, which this machine does not have"
            )));
        }
    }
    match Command::new("/usr/bin/unshare")
        .args(["--mount", "true"])
        .output()
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => Err(IsolationError::new(
            "the testbed isolation cannot create a mount namespace on this machine",
        )),
        Err(error) => Err(IsolationError::new(format!(
            "the testbed isolation could not run unshare: {error}"
        ))),
    }
}
