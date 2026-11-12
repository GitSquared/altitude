use std::{
    env,
    error::Error as StdError,
    fmt, fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use tokio::sync::{mpsc, watch};

use crate::project::Project;
use serde::{Deserialize, Serialize};

// How much of this host a sandbox may take, and how many may run at once. It
// lives here rather than beside the task queue because it is a fact about the
// machine, not about the work.
pub(crate) mod capacity;

pub use capacity::{PROJECT_CACHE_QUOTA_MIB, Pressure, STORE_QUOTA_MIB, SandboxRoom, landings};
mod git_cache;
pub(crate) mod mounts;
mod providers;
mod relay;
mod sandbox;

pub(crate) use providers::workers::protocol as worker_protocol;
pub use providers::{IsolationBackend, MicrosandboxIsolation, TestbedIsolation};
pub(crate) use relay::{PORT_RELAY_GUEST_PORT, install_script, with_port_relay};
pub use relay::{PortNotice, PortRelays};
pub(crate) use sandbox::guest_absolute;
pub(crate) use sandbox::private_directory;
pub use sandbox::{DefaultFile, GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand};
pub(crate) use sandbox::{run_collecting, shell_quote};

// A command's search path inside a sandbox. `/root` is the guest's home whatever
// account the host runs as, because a host engine maps it onto the sandbox's own.
pub(crate) const GUEST_PATH: &str = concat!(
    "/root/.cargo/bin:/root/.local/bin",
    ":/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
);

#[derive(Clone, Debug)]
pub enum IsolationStatus {
    Starting,
    InstallingRuntime,
    PreparingBaseImage(ImagePull),
    PreparingBasePackages(PackageInstall),
    Ready,
    Failed(String),
}

// How far the base image has come down. The bytes are unknown until the manifest
// is read, and the image is fetched only once, so both halves may be zero.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImagePull {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
}

// How far an environment's installs have come. dpkg names each package as it sets it
// up, and the count only grows; the name is the one being configured now. There is no
// total: a build runs apt more than once, and a denominator that grows with each run
// is a number that promises an end it keeps moving.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageInstall {
    pub installed: u64,
    pub current: Option<String>,
}

pub struct CommandSession {
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: mpsc::UnboundedReceiver<CommandEvent>,
}

#[derive(Clone, Debug)]
pub struct BaseSandboxRecipe {
    pub name: String,
    pub snapshots_dir: PathBuf,
    pub refresh_after: Duration,
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

    pub fn setup_command(&self) -> String {
        self.steps
            .iter()
            .map(|step| step.command.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetupStep {
    pub label: String,
    pub command: String,
}

#[derive(Clone)]
pub struct CredentialSandbox {
    pub id: String,
    pub recipes: Vec<ProfileRecipe>,
    pub snapshot: Option<String>,
    pub vsock: Vec<VsockRoute>,
}

impl SetupStep {
    pub fn new(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EnvironmentProgress {
    pub label: String,
    pub completed: usize,
    pub total: usize,
}

pub trait EnvironmentProgressSink: Send + Sync {
    fn report(&self, progress: EnvironmentProgress);
}

pub trait PackageInstallSink: Send + Sync {
    fn installing(&self, install: PackageInstall);
}

pub struct Unwatched;

impl EnvironmentProgressSink for Unwatched {
    fn report(&self, _progress: EnvironmentProgress) {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTally {
    pub files: u64,
    pub bytes: u64,
}

// A tree moves in one step now, so a sink is told how big it is rather than how much
// of it has happened.
pub trait FileTransferSink: Send + Sync {
    fn moving(&self, whole: FileTally);
}

pub struct UnwatchedFiles;

impl FileTransferSink for UnwatchedFiles {
    fn moving(&self, _whole: FileTally) {}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseSandboxSnapshot {
    pub path: PathBuf,
    pub created_at_ms: i64,
    pub os_release: String,
    pub packages: String,
    pub setup_command: String,
}

#[derive(Clone, Debug)]
pub struct ProfileSummary {
    pub profile: String,
    pub display: &'static str,
    pub sign_in: &'static str,
    pub sign_in_command: SignInCommand,
}

pub const CREDENTIAL_PREFIX: &str = "credentials-";

pub const WATCHER_PREFIX: &str = "watcher-";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CredentialReport {
    pub output: String,
    pub signed_in: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CredentialCheck {
    pub profile: String,
    pub command: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentDescription {
    pub os_release: String,
    pub packages: Vec<String>,
    pub built_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxOptions {
    pub snapshot_path: Option<String>,
    pub network_enabled: bool,
    pub mounts: Vec<SandboxMount>,
    pub environment: Vec<(String, String)>,
    pub vsock: Vec<VsockRoute>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxPurpose {
    Capsule,
    Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCeiling {
    pub cpus: u8,
    pub memory_mib: u32,
    pub disk_mib: u32,
}

impl SandboxCeiling {
    pub const COMMAND: Self = Self {
        cpus: 1,
        memory_mib: 512,
        disk_mib: 15 * 1_024,
    };
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VsockRoute {
    pub host_socket: PathBuf,
    pub guest_port: u32,
}

pub(crate) fn mount_git_cache(options: &mut SandboxOptions, cache_dir: &Path) {
    match git_cache::ready(cache_dir) {
        Ok(host_path) => {
            options.mounts.push(SandboxMount {
                host_path,
                guest_path: git_cache::GUEST_PATH.to_owned(),
                readonly: false,
                noexec: false,
                quota_mib: git_cache::GUEST_QUOTA_MIB,
            });
            options.environment.push((
                git_cache::ENVIRONMENT.to_owned(),
                git_cache::GUEST_PATH.to_owned(),
            ));
        }
        Err(error) => tracing::warn!(%error, "no git cache for this sandbox"),
    }
}

pub(crate) fn trust_host_roots(options: &mut SandboxOptions, data_dir: &Path) -> bool {
    let roots: Vec<PathBuf> = ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "CURL_CA_BUNDLE"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .collect();
    if roots.is_empty() {
        return false;
    }
    let mut bundle = String::new();
    for root in &roots {
        match fs::read_to_string(root) {
            Ok(text) => {
                bundle.push_str(&text);
                if !bundle.ends_with('\n') {
                    bundle.push('\n');
                }
            }
            Err(error) => {
                tracing::warn!(%error, path = %root.display(), "could not read a trusted root")
            }
        }
    }
    if bundle.is_empty() {
        return false;
    }
    // Somewhere without spaces in the path: a sandbox mount cannot open one that has
    // them, and an endpoint agent's own root lives under a directory that does.
    let staged = data_dir.join("host-roots").join("host-roots.pem");
    if let Some(parent) = staged.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        tracing::warn!(%error, "could not keep this machine's trusted roots");
        return false;
    }
    if let Err(error) = fs::write(&staged, bundle) {
        tracing::warn!(%error, "could not keep this machine's trusted roots");
        return false;
    }
    let guest = "/etc/altitude/host-roots.pem";
    options.mounts.push(SandboxMount {
        host_path: staged.clone(),
        guest_path: guest.to_owned(),
        readonly: true,
        noexec: true,
        quota_mib: STORE_QUOTA_MIB,
    });
    options
        .environment
        .push(("NODE_EXTRA_CA_CERTS".to_owned(), guest.to_owned()));
    // For the tools that read the system store, and for scripts that only look here.
    options
        .environment
        .push(("ALTITUDE_HOST_ROOTS".to_owned(), guest.to_owned()));
    true
}

impl SandboxOptions {
    pub fn isolated(snapshot_path: Option<String>) -> Self {
        Self {
            snapshot_path,
            network_enabled: false,
            mounts: Vec::new(),
            environment: Vec::new(),
            vsock: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
// Nothing a guest can write is mounted from the host: mounted files travel through
// `sandbox.fs()` one at a time, because a guest-writable host directory is a way out
// of the sandbox and a place to plant a hook the host will run.
pub struct SandboxMount {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub readonly: bool,
    pub noexec: bool,
    // Stated by every mount rather than left to the runtime's own default, which is
    // sized for something that must not fill a host disk and not for a cache.
    pub quota_mib: u32,
}

// What a layer's steps run in: the mounts and the variables the sandbox's commands
// will have. A profile that writes its own configuration has to write it where the
// task will read it, so a step that runs outside this view prepares a sandbox it
// cannot fill.
pub struct LayerView {
    pub mounts: Vec<SandboxMount>,
    pub environment: Vec<(String, String)>,
}

// Where a machine that is its own base records that a layer's steps have already run
// on it: a note rather than an image, in the layer's own snapshot directory.
pub(crate) const LAYER_MARKER: &str = "testbed-built";

// The installs layer a host built, kept somewhere the next host can find it. A host
// with nowhere to put one builds every layer itself, as it always did.
pub trait HostLayers: Send + Sync {
    // The archive for this host's own environment, if one is held.
    fn fetch<'a>(
        &'a self,
        tag: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<PathBuf>> + Send + 'a>>;
    // This host's copy, for whoever comes next. A failure leaves the layer built and
    // the next host to build its own, so it is logged rather than fatal.
    fn store<'a>(
        &'a self,
        tag: &'a str,
        archive: PathBuf,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

// What the image a host runs is, when its operator says so. A layer is built on a
// machine and is only valid for that machine, so the tag it is stored under has to
// name what the machine is: an image that changes invalidates it by construction,
// and a host that declares nothing keeps the tags it already has.
pub(crate) fn environment_identity() -> Option<String> {
    env::var("ALTITUDE_WORKER_ENVIRONMENT")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Interrupt,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CommandEvent {
    Output {
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    Exited(i32),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub struct IsolationError {
    message: String,
    is_missing_sandbox: bool,
}

impl IsolationError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_missing_sandbox: false,
        }
    }

    // The record named a sandbox the runtime has never heard of, which a hard stop
    // leaves behind for every task that was running. Callers that only wanted to
    // reach that sandbox are done rather than refused.
    pub(crate) fn missing_sandbox(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_missing_sandbox: true,
        }
    }

    pub(crate) fn is_missing_sandbox(&self) -> bool {
        self.is_missing_sandbox
    }
}

impl fmt::Display for IsolationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for IsolationError {}

pub trait IsolationEngine: Send + Sync {
    // Whether the machine is coping, and whether asking it is a question that a
    // sample has to be taken for.
    fn pressure(&self) -> Option<Pressure>;

    fn observe_pressure(&self) -> Option<Pressure>;

    fn sandbox_room(&self) -> SandboxRoom;

    // How many sandboxes may be starting at once. A host engine counts cores,
    // because a start is minutes of boot before it shows in the load and two at
    // a time is what a machine absorbs; a provider that spawns compute elsewhere
    // counts what it is allowed to run at all.
    fn start_landings(&self) -> usize {
        landings()
    }

    // Where a project's memory lives on this machine. It comes from the engine rather
    // than from the caller's config because the engine is the piece that knows
    // whether this machine scopes storage per operator, and the sandbox mount and the
    // sync have to agree on one directory.
    fn project_memory_dir(&self, project_id: &str) -> PathBuf;

    fn provision(
        &self,
        id: String,
        task_snapshot: Option<String>,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        vsock: Vec<VsockRoute>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn for_operator(&self, owner: &str) -> Result<Self, IsolationError>
    where
        Self: Sized + Clone,
    {
        let _ = owner;
        Ok(self.clone())
    }

    fn credential_shell(
        &self,
        sandbox: CredentialSandbox,
        profile: &str,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<String, IsolationError>> + Send;

    fn watcher(
        &self,
        id: String,
        project: Project,
        recipes: Vec<ProfileRecipe>,
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
        purpose: SandboxPurpose,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn ensure_project(
        &self,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<BaseSandboxSnapshot, IsolationError>> + Send;

    // The layers are bottom first, and each is built on the one before it: the
    // machine's own software does not have to be installed again because a project
    // changed what it wants on top.
    fn ensure_base(
        &self,
        layers: Vec<BaseSandboxRecipe>,
        view: LayerView,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<BaseSandboxSnapshot, IsolationError>> + Send;

    fn base_description(&self) -> Option<EnvironmentDescription>;

    fn run_command(
        &self,
        id: String,
        command: String,
    ) -> impl Future<Output = Result<CommandSession, IsolationError>> + Send;

    fn project_snapshot(&self, project: Project, recipes: Vec<ProfileRecipe>) -> Option<String>;

    fn credential_reports(
        &self,
        sandbox: CredentialSandbox,
        checks: Vec<CredentialCheck>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<Option<Vec<(String, CredentialReport)>>, IsolationError>> + Send;

    fn remove_leftover_sandboxes(&self) -> impl Future<Output = ()> + Send;

    fn remove(&self, id: String) -> impl Future<Output = Result<(), IsolationError>> + Send;

    // Whether the runtime still has this sandbox. A record can name one that a hard
    // stop took with it, and asking is cheaper than building a new one to find out.
    fn has_sandbox(&self, id: String) -> impl Future<Output = bool> + Send;

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

    fn collect_files(
        &self,
        id: String,
        root: String,
        excluded: Vec<String>,
        destination: PathBuf,
        progress: &dyn FileTransferSink,
    ) -> impl Future<Output = Result<FileTally, IsolationError>> + Send;

    fn collect_file(
        &self,
        id: String,
        guest_path: String,
        host_path: PathBuf,
    ) -> impl Future<Output = Result<u64, IsolationError>> + Send;

    fn deliver_file(
        &self,
        id: String,
        host_path: PathBuf,
        guest_path: String,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;

    fn deliver_files(
        &self,
        id: String,
        source: PathBuf,
        root: String,
        progress: &dyn FileTransferSink,
    ) -> impl Future<Output = Result<FileTally, IsolationError>> + Send;
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
