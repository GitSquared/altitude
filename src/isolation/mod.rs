use std::{
    env,
    error::Error as StdError,
    fmt, fs,
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::sync::{mpsc, watch};

use crate::project::Project;

// How much of this host a sandbox may take, and how many may run at once. It
// lives here rather than beside the task queue because it is a fact about the
// machine, not about the work.
pub(crate) mod capacity;

pub use capacity::SandboxCapacity;
mod git_cache;
mod microsandbox;
pub(crate) mod mounts;
mod relay;
mod sandbox;

pub use microsandbox::MicrosandboxIsolation;
pub(crate) use relay::{PORT_RELAY_GUEST_PORT, install_script, with_port_relay};
pub use relay::{PortNotice, PortRelays};
pub(crate) use sandbox::guest_absolute;
pub(crate) use sandbox::private_directory;
pub use sandbox::{DefaultFile, GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand};
pub(crate) use sandbox::{run_collecting, shell_quote};

#[derive(Clone, Debug)]
pub enum IsolationStatus {
    Starting,
    InstallingRuntime,
    PreparingBaseImage,
    PreparingBasePackages,
    Ready,
    Failed(String),
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

#[derive(Clone, Debug)]
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

pub struct Unwatched;

impl EnvironmentProgressSink for Unwatched {
    fn report(&self, _progress: EnvironmentProgress) {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileTally {
    pub files: u64,
    pub bytes: u64,
}

pub trait FileTransferSink: Send + Sync {
    fn counted(&self, whole: FileTally);
    fn moved(&self, step: FileTally);
}

pub struct UnwatchedFiles;

impl FileTransferSink for UnwatchedFiles {
    fn counted(&self, _whole: FileTally) {}
    fn moved(&self, _step: FileTally) {}
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct CredentialReport {
    pub output: String,
    pub signed_in: bool,
}

#[derive(Clone, Debug)]
pub struct CredentialCheck {
    pub profile: String,
    pub command: String,
}

#[derive(Clone, Debug)]
pub struct EnvironmentDescription {
    pub os_release: String,
    pub packages: Vec<String>,
    pub built_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SandboxOptions {
    pub snapshot_path: Option<String>,
    pub network_enabled: bool,
    pub mounts: Vec<SandboxMount>,
    pub environment: Vec<(String, String)>,
    pub vsock: Vec<VsockRoute>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxPurpose {
    Capsule,
    Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
// Nothing a guest can write is mounted from the host: mounted files travel through
// `sandbox.fs()` one at a time, because a guest-writable host directory is a way out
// of the sandbox and a place to plant a hook the host will run.
pub struct SandboxMount {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub readonly: bool,
    pub noexec: bool,
}

pub enum Command {
    Input(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Interrupt,
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
    fn sandbox_capacity(&self) -> SandboxCapacity;

    fn observe_demand(&self, queued_tasks: usize) -> SandboxCapacity;

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

    fn ensure_base(
        &self,
        recipe: BaseSandboxRecipe,
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
