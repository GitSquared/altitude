use std::{
    env,
    error::Error as StdError,
    fmt, fs,
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::sync::{mpsc, watch};

pub(crate) mod capsule;
mod environment;
mod microsandbox;
/// How long a built environment is kept before it is rebuilt, for the interface
/// that has to tell an operator when their base was last refreshed.
pub use environment::REFRESH_AFTER;
/// Shared with the VCS providers so capsule commands are quoted one way.
pub(crate) use environment::shell_quote;
pub use environment::{
    BASE_NAME, DefaultFile, Environment, EnvironmentRecipe, GuestPath, PersistentDirectory,
    is_base_name,
};
pub use microsandbox::MicrosandboxIsolation;

#[derive(Clone, Debug)]
pub enum IsolationStatus {
    Starting,
    InstallingRuntime,
    PreparingBaseImage,
    PreparingWorkspacePackages,
    Ready,
    Failed(String),
}

pub struct CommandSession {
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: mpsc::UnboundedReceiver<CommandEvent>,
}

/// The composed recipe for a reusable sandbox environment.
///
/// Isolation owns image refreshes, snapshot promotion, and cleanup. An agent
/// contributes package installation and persistent configuration requirements.
#[derive(Clone, Debug)]
pub struct BaseSandboxRecipe {
    pub name: String,
    pub snapshots_dir: PathBuf,
    pub refresh_after: Duration,
    /// Run in order, each in its own shell. Their commands are also this
    /// recipe's identity: a snapshot is reused only while they are unchanged.
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

    /// The work this recipe describes, as one value to compare and persist.
    pub fn setup_command(&self) -> String {
        self.steps
            .iter()
            .map(|step| step.command.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One command in an environment build, named so that an operator waiting on
/// the build can see what the wait is for.
#[derive(Clone, Debug)]
pub struct SetupStep {
    pub label: String,
    pub command: String,
}

impl SetupStep {
    pub fn new(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
        }
    }
}

/// What an environment build is doing, for the task that waits on it.
#[derive(Clone, Debug)]
pub struct EnvironmentProgress {
    /// The step in progress, in the operator's words.
    pub label: String,
    /// Steps finished before this one, and the build's step count.
    pub completed: usize,
    pub total: usize,
}

/// Receives build progress. Isolation reports it; the task layer decides where
/// it goes, and a build nobody waits on can ignore it.
pub trait EnvironmentProgressSink: Send + Sync {
    fn report(&self, progress: EnvironmentProgress);
}

/// A build nobody is watching, for when a caller has no task to report progress to.
pub struct Unwatched;

impl EnvironmentProgressSink for Unwatched {
    fn report(&self, _progress: EnvironmentProgress) {}
}

#[derive(Clone, Debug)]
pub struct BaseSandboxSnapshot {
    pub path: PathBuf,
    pub created_at_ms: i64,
    /// What the snapshot reported about its image and packages, empty when it
    /// could not be asked. See [`EnvironmentDescription`].
    pub alpine_release: String,
    pub packages: String,
    pub setup_command: String,
}

/// A profile a task mounts into its sandbox: where it lives on this machine, and
/// where a task sees it.
///
/// A profile is how a login survives between tasks, so this is the shape of the
/// answer to "what identity is logged in": the names and the places, never the
/// contents.
#[derive(Clone, Debug)]
pub struct ProfileSummary {
    /// The recipe's stable profile identity, e.g. `agents/claude`.
    pub profile: &'static str,
    /// Each mounted directory, as its path inside the profile and where a task
    /// finds it.
    pub mounts: Vec<ProfileMount>,
}

#[derive(Clone, Debug)]
pub struct ProfileMount {
    pub path: &'static str,
    pub guest: String,
}

/// Where a task finds a mounted directory.
pub fn guest_path(guest: &GuestPath) -> String {
    match guest {
        GuestPath::Absolute(path) => (*path).to_owned(),
        GuestPath::HomeRelative(path) => format!("~/{path}"),
    }
}

/// What a built environment says about itself.
///
/// Read back from the snapshot rather than derived from configuration, because
/// it records what was installed rather than what was asked for. Nothing else
/// can answer this: the image is a rolling tag and the setup commands name no
/// versions.
#[derive(Clone, Debug)]
pub struct EnvironmentDescription {
    /// The image's release, e.g. `3.21.3`.
    pub alpine_release: String,
    /// Installed packages as `name-version`, in the order the build listed them.
    pub packages: Vec<String>,
    pub built_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SandboxOptions {
    pub snapshot_path: Option<String>,
    pub(crate) environment_base: Option<capsule::EnvironmentBase>,
    pub network_enabled: bool,
    pub mounts: Vec<SandboxMount>,
    pub environment: Vec<(String, String)>,
}

/// Gives a sandbox the extra roots this machine trusts.
///
/// A machine whose traffic is inspected — by an endpoint agent, or a proxy — verifies
/// against roots no base image has, and a task that cannot verify a registry cannot
/// install anything. The roots are collected into one bundle and mounted into every
/// sandbox, build or run, rather than built into a snapshot: a snapshot can be moved
/// to another machine, and one host's inspection root must never travel to another.
///
/// `NODE_EXTRA_CA_CERTS` adds to a store rather than replacing it, which is why it is
/// safe here where `SSL_CERT_FILE` would not be. Tools that read the system store —
/// curl, apk, git — need the bundle appended to that store, which a build step does.
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
            environment_base: None,
            network_enabled: false,
            mounts: Vec::new(),
            environment: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SandboxMount {
    pub host_path: PathBuf,
    pub guest_path: String,
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
pub struct IsolationError(String);

impl IsolationError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
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
    fn provision(
        &self,
        id: String,
        task_snapshot: Option<String>,
        environment: Environment,
        recipes: Vec<EnvironmentRecipe>,
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
    ) -> impl Future<Output = Result<(), IsolationError>> + Send;

    /// Build an environment's snapshot if it is not already here, and say where it is.
    ///
    /// A capsule travelling from another machine leaves out the environment it was
    /// built on, because every machine builds that environment for itself from the same
    /// configuration. Landing one therefore needs the environment here first — the work
    /// a task would do on its first run, done ahead of the capsule rather than by it —
    /// and the snapshot it lands on is what resolves the layers that stayed behind.
    fn ensure_environment(
        &self,
        environment: Environment,
        recipes: Vec<EnvironmentRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<BaseSandboxSnapshot, IsolationError>> + Send;

    fn ensure_base(
        &self,
        recipe: BaseSandboxRecipe,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<BaseSandboxSnapshot, IsolationError>> + Send;

    /// What the workspace base says about itself, if it has been built.
    ///
    /// Synchronous because it reads a manifest rather than a machine: the answer
    /// was recorded when the snapshot was built, because only a live sandbox can
    /// say it and by the time an operator asks there is none.
    fn base_description(&self) -> Option<EnvironmentDescription>;

    fn run_command(
        &self,
        id: String,
        command: String,
    ) -> impl Future<Output = Result<CommandSession, IsolationError>> + Send;

    fn remove(&self, id: String) -> impl Future<Output = Result<(), IsolationError>> + Send;

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
