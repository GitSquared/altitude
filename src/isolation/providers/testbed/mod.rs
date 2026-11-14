use std::{
    env, fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Instant,
};

use serde::{Deserialize, Serialize};
use tar::{Archive, Builder};
use tempfile::NamedTempFile;
use tokio::task::spawn_blocking;
// `tokio`'s, not `std`'s, because it is held across running a step.
use tokio::sync::Mutex as LayerBuild;

use crate::{
    config::Config,
    isolation::{
        BaseSandboxRecipe, BaseSandboxSnapshot, EnvironmentProgress, EnvironmentProgressSink,
        HostLayers, IsolationError, LAYER_MARKER, LayerView, SetupStep, capacity::HostPressure,
        private_directory,
    },
};

mod engine;
mod files;
mod machine;

// The sandbox directory a layer's steps run in: the view, without a task in it.
const LAYER_SANDBOX: &str = "layer";

// The directories every sandbox is given over the machine rather than from its own
// disk. A command resolves paths against these, so the same absolute path means the
// machine's file here and now.
const SYSTEM_ROOTS: [&str; 6] = ["/usr", "/etc", "/dev", "/sys", "/proc", "/run"];

// A weaker isolation primitive for running Altitude inside one of its own sandboxes,
// where a nested microVM has no `/dev/kvm` to boot against. Each sandbox is a
// directory used as a `chroot` root, and every command runs in a private user and
// mount namespace that binds the machine's system trees and the plan's mounts over
// it. It is not a boundary: a testbed task shares the machine's `/usr`, and its
// home is the account's own, which it can write. That is the point — the machine is
// already a disposable sandbox, and none of it needs root.
#[derive(Clone)]
pub struct TestbedIsolation {
    config: Config,
    // The account's home, which stands in for the guest's `/root`: on a disposable
    // host the agent's home is the worker account's, so it can write it without root.
    home: PathBuf,
    // Shared rather than per view, like the microVM provider's: how much this
    // machine can take is the same answer for every operator on it.
    host_pressure: Arc<Mutex<HostPressure>>,
    // A layer's steps run in a sandbox directory of their own, so two layers
    // building at once would share one view and each see the other's work.
    layer_build: Arc<LayerBuild<()>>,
    // Where a built installs layer goes, when this host has somewhere to leave it.
    layers: Option<Arc<dyn HostLayers>>,
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
            home: env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/root")),
            host_pressure: Arc::new(Mutex::new(HostPressure::new(&config.isolation_data_dir()))),
            layer_build: Arc::new(LayerBuild::new(())),
            layers: None,
        }
    }

    // A host that can leave what it builds, and restore what another host left.
    pub fn with_host_layers(mut self, layers: Arc<dyn HostLayers>) -> Self {
        self.layers = Some(layers);
        self
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
        // The guest's home is the account's own: on a disposable host there is no
        // separate root-owned `/root` to bind, and the agent has to be able to write.
        if guest == "/root" {
            return self.home.clone();
        }
        if let Some(suffix) = guest.strip_prefix("/root/") {
            return self.home.join(suffix);
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
        view: &LayerView,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        let marker = layer.snapshots_dir.join(LAYER_MARKER);
        if marker.is_file() {
            return Ok(Self::layer_snapshot(marker, &layer));
        }
        // An installs layer is the host's own software, and what it writes is a home
        // directory: every profile installs into $HOME and nowhere else. So a host
        // that finds it already built restores it instead of running the commands.
        let installs_tag = self.installs_layer_tag(&layer);
        if let (Some(layers), Some(tag)) = (&self.layers, installs_tag.as_deref())
            && let Some(archive) = layers.fetch(tag).await
        {
            match self.restore_home(&archive).await {
                Ok(()) => {
                    private_directory(&layer.snapshots_dir)?;
                    fs::write(&marker, b"built\n")
                        .map_err(|error| IsolationError::new(error.to_string()))?;
                    tracing::info!(tag, "restored an installs layer another host built");
                    let _ = fs::remove_file(&archive);
                    return Ok(Self::layer_snapshot(marker, &layer));
                }
                // A layer that will not unpack is not a reason to fail the task: build
                // it, as this host would have without the store.
                Err(error) => {
                    tracing::warn!(%error, tag, "a stored installs layer would not restore");
                    let _ = fs::remove_file(&archive);
                }
            }
        }
        private_directory(&layer.snapshots_dir)?;
        let steps: Vec<&SetupStep> = layer
            .steps
            .iter()
            .filter(|step| !step.command.trim().is_empty())
            .collect();
        // One build at a time: the steps of two layers would share the sandbox
        // directory they run in and each see the other's work.
        {
            let _build = self.layer_build.lock().await;
            self.run_layer(view, &steps, progress).await?;
        }
        fs::write(&marker, b"built\n").map_err(|error| IsolationError::new(error.to_string()))?;
        if let (Some(layers), Some(tag)) = (&self.layers, installs_tag.as_deref())
            && let Err(error) = self.store_home(layers, tag).await
        {
            // The layer is built and this host is ready; the next host simply builds
            // its own, which is what every host did before any of this existed.
            tracing::warn!(%error, tag, "could not leave this installs layer for the next host");
        }
        Ok(Self::layer_snapshot(marker, &layer))
    }

    // A layer's steps, in a sandbox directory of their own and in the view the
    // sandbox's own commands will have: a step that writes its config into $HOME has
    // to write it where the task will read it.
    async fn run_layer(
        &self,
        view: &LayerView,
        steps: &[&SetupStep],
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        let root = self.sandboxes_dir().join(LAYER_SANDBOX);
        machine::prepare_root(&root)?;
        let record = SandboxRecord {
            mounts: view
                .mounts
                .iter()
                .map(|mount| MountRecord {
                    host_path: mount.host_path.clone(),
                    guest_path: mount.guest_path.clone(),
                    readonly: mount.readonly,
                })
                .collect(),
            environment: view.environment.clone(),
        };
        for (completed, step) in steps.iter().enumerate() {
            tracing::info!(
                step = %step.label,
                completed,
                total = steps.len(),
                "building an installs layer"
            );
            progress.report(EnvironmentProgress {
                label: step.label.clone(),
                completed,
                total: steps.len(),
            });
            machine::run_step(&root, &record, &step.command, &self.home)
                .await
                .map_err(|error| IsolationError::new(format!("{}: {error}", step.label)))?;
        }
        if let Err(error) = fs::remove_dir_all(&root) {
            tracing::warn!(
                %error,
                path = %root.display(),
                "could not remove the layer build's sandbox directory"
            );
        }
        Ok(())
    }

    // The machine is the base, so a layer's snapshot is the note that its steps ran:
    // there is no image altitude built, only a marker in the layer's own directory.
    fn layer_snapshot(marker: PathBuf, layer: &BaseSandboxRecipe) -> BaseSandboxSnapshot {
        BaseSandboxSnapshot {
            path: marker,
            created_at_ms: now_ms(),
            os_release: os_release(),
            packages: String::new(),
            setup_command: layer.setup_command(),
        }
    }

    // What is worth handing on is the installs layer: the agent and version-control
    // software, the same whatever project runs. A project's own layer builds its
    // environment inside the task's checkout, which belongs to that task.
    fn installs_layer_tag(&self, layer: &BaseSandboxRecipe) -> Option<String> {
        let machine = self.config.installs_snapshots_dir();
        if layer.snapshots_dir.parent() != Some(machine.as_path()) {
            return None;
        }
        layer
            .snapshots_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    // The home as this host holds it, which is the whole of an installs layer: the
    // profile directories and project caches are mounted into the layer's view from
    // the master, so taking the home from outside that view leaves them out.
    async fn store_home(
        &self,
        layers: &Arc<dyn HostLayers>,
        tag: &str,
    ) -> Result<(), IsolationError> {
        let entry = self.home_name()?;
        let archive = NamedTempFile::new().map_err(io_error)?;
        let packing = archive.path().to_path_buf();
        let home = self.home.clone();
        let began = Instant::now();
        spawn_blocking(move || -> Result<(), String> {
            let file = fs::File::create(&packing).map_err(|error| error.to_string())?;
            let mut builder = Builder::new(file);
            builder
                .append_dir_all(&entry, &home)
                .map_err(|error| error.to_string())?;
            builder.finish().map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| IsolationError::new(error.to_string()))?
        .map_err(IsolationError::new)?;
        tracing::info!(
            tag,
            elapsed_ms = began.elapsed().as_millis() as u64,
            "packed this host's home into a layer"
        );
        layers.store(tag, archive.path().to_path_buf()).await;
        Ok(())
    }

    async fn restore_home(&self, archive: &Path) -> Result<(), IsolationError> {
        let parent = self.home_parent()?;
        let stored = archive.to_path_buf();
        let began = Instant::now();
        spawn_blocking(move || -> Result<(), String> {
            let file = fs::File::open(&stored).map_err(|error| error.to_string())?;
            let mut tar = Archive::new(file);
            tar.unpack(&parent).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| IsolationError::new(error.to_string()))?
        .map_err(IsolationError::new)?;
        tracing::info!(
            elapsed_ms = began.elapsed().as_millis() as u64,
            "unpacked an installs layer into this host's home"
        );
        Ok(())
    }

    // An archive of the home holds one top-level directory, named after the home
    // itself, so unpacking it into the home's parent puts it back where it was.
    fn home_name(&self) -> Result<String, IsolationError> {
        self.home
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| IsolationError::new("this host's home has no name to archive"))
    }

    fn home_parent(&self) -> Result<PathBuf, IsolationError> {
        self.home
            .parent()
            .map(Path::to_path_buf)
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| IsolationError::new("this host's home has no parent"))
    }

    // A host that is about to be destroyed has no machine to come back to, so a
    // snapshot is the sandbox itself: the tree it wrote, and the project checkout,
    // which lives outside it under the machine's own roots.
    pub(crate) fn archive_sandbox(
        &self,
        id: &str,
        checkout: Option<&str>,
        destination: &Path,
    ) -> Result<(), IsolationError> {
        let root = self.sandbox_root(id);
        if !root.is_dir() {
            return Err(IsolationError::missing_sandbox(format!(
                "this testbed sandbox is not on disk: {id}"
            )));
        }
        let file = fs::File::create(destination).map_err(io_error)?;
        let mut builder = Builder::new(file);
        builder.append_dir_all("root", &root).map_err(io_error)?;
        if let Some(checkout) = checkout {
            let record = self.read_record(id)?;
            let host = self.host_path(id, &record, checkout);
            if host.exists() {
                builder
                    .append_dir_all("checkout", &host)
                    .map_err(io_error)?;
            }
        }
        builder.finish().map_err(io_error)?;
        Ok(())
    }

    pub(crate) fn restore_sandbox(
        &self,
        id: &str,
        checkout: Option<&str>,
        source: &Path,
    ) -> Result<(), IsolationError> {
        let root = self.sandbox_root(id);
        let checkout_host = match checkout {
            Some(checkout) => {
                let record = self.read_record(id)?;
                Some(self.host_path(id, &record, checkout))
            }
            None => None,
        };
        let file = fs::File::open(source).map_err(io_error)?;
        let mut archive = Archive::new(file);
        for entry in archive.entries().map_err(io_error)? {
            let mut entry = entry.map_err(io_error)?;
            let path = entry.path().map_err(io_error)?.into_owned();
            let mut components = path.components();
            let base = components.next();
            let rest = components.as_path();
            let target = match base {
                Some(Component::Normal(name)) if name == "root" => root.join(rest),
                Some(Component::Normal(name)) if name == "checkout" && checkout_host.is_some() => {
                    checkout_host.as_ref().expect("checked above").join(rest)
                }
                _ => continue,
            };
            entry.unpack(&target).map_err(io_error)?;
        }
        Ok(())
    }
}

fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
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

pub(super) fn check_environment() -> Result<(), IsolationError> {
    for binary in ["/usr/bin/unshare", "/usr/sbin/chroot", "/usr/bin/mount"] {
        if !Path::new(binary).is_file() {
            return Err(IsolationError::new(format!(
                "the testbed isolation needs {binary}, which this machine does not have"
            )));
        }
    }
    match Command::new("/usr/bin/unshare")
        .args(["--user", "--map-root-user", "--mount", "true"])
        .output()
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => Err(IsolationError::new(
            "the testbed isolation cannot create a user and mount namespace on this machine",
        )),
        Err(error) => Err(IsolationError::new(format!(
            "the testbed isolation could not run unshare: {error}"
        ))),
    }
}
