use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::symlink,
    path::PathBuf,
    process,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use microsandbox::{
    LocalBackend, Sandbox,
    sandbox::{PullPolicy, exec::ExecEvent},
    set_default_backend,
    setup::{InstallOptions, ensure_runtime},
    snapshot::Snapshot,
};
use tokio::sync::{Mutex, mpsc, watch};

use crate::config::Config;
use crate::development::DevelopmentEnvironment;

use super::{
    BaseSandboxRecipe, BaseSandboxSnapshot, Command, CommandEvent, CommandSession,
    EnvironmentDescription, EnvironmentProgress, EnvironmentProgressSink, EnvironmentRecipe,
    IsolationEngine, IsolationError, IsolationStatus, OutputStream, SandboxOptions, SetupStep,
    environment::{self, REFRESH_AFTER, private_directory},
};

const BASE_IMAGE: &str = "alpine";

/// What one capsule is created with.
///
/// Ceilings the sandbox enforces, not reservations: most tasks use a fraction of
/// them most of the time, which is why how many run at once is observed rather
/// than derived from these. Deliberately not configurable per environment — one
/// allocation for every capsule, for now — and the values are still arbitrary.
const CAPSULE_CPUS: u8 = 4;
const CAPSULE_MEMORY_MIB: u32 = 5 * 1_024;
const CAPSULE_DISK_MIB: u32 = 15 * 1_024;

/// The packages every sandbox gets, before any environment adds its own.
///
/// Named once, because two things derive from the list: the command that
/// installs them, and the command that asks a finished snapshot what versions it
/// ended up with. The image's own plumbing is deliberately absent — it is true
/// of every sandbox and tells an operator nothing.
const WORKSPACE_PACKAGES: &[&str] = &[
    "bash",
    "curl",
    "python3",
    "py3-pip",
    "jq",
    "coreutils",
    "findutils",
    "sed",
    "grep",
    "nodejs",
    "npm",
    "ripgrep",
    "libgcc",
    "libstdc++",
];

/// What the workspace is built from, as the build runs it.
fn workspace_setup() -> String {
    format!(
        "set -eu\napk upgrade --no-cache\napk add --no-cache {}\ncat > /root/README.md <<'ALTITUDE_WORKSPACE_README'\n{}ALTITUDE_WORKSPACE_README\n",
        WORKSPACE_PACKAGES.join(" "),
        include_str!("assets/workspace.md"),
    )
}
const PREPARATION_SANDBOX: &str = "altitude-base-image-setup";

/// What a finished snapshot is made of, asked inside it.
///
/// One command, so the answer is one round trip, and one `key<TAB>value` line
/// each, so the manifest can hold them on lines of its own. Recorded rather than
/// inferred: the base image is a rolling tag and the setup commands name no
/// versions, so nothing outside the snapshot can say what an operator ended up
/// with.
///
/// The packages are the ones the workspace asked for, with their versions,
/// rather than everything present: the image's own plumbing is true of every
/// sandbox and tells an operator nothing, while the version of Python they got
/// is worth knowing.
fn description_command() -> String {
    // `apk info -v` with no arguments lists every installed package as
    // `name-version`; the list is filtered to the ones the workspace asked for,
    // because the image's own plumbing is true of every sandbox and tells an
    // operator nothing.
    format!(
        "printf 'alpine\\t%s\\n' \"$(cat /etc/alpine-release 2>/dev/null || echo unknown)\"\nprintf 'packages\\t%s\\n' \"$(apk info -v 2>/dev/null | grep -E '^({})-[0-9]' | tr '\\n' ' ')\"\n",
        WORKSPACE_PACKAGES.join("|"),
    )
}

#[derive(Clone)]
pub struct MicrosandboxIsolation {
    config: Config,
    data_home: PathBuf,
    cache_home: PathBuf,
    // Microsandbox's Unix sockets exceed macOS path limits under the longer data home.
    socket_safe_home: PathBuf,
    base_preparation_lock: Arc<Mutex<()>>,
}

impl MicrosandboxIsolation {
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            data_home: config.isolation_data_dir(),
            cache_home: config.isolation_cache_dir(),
            socket_safe_home: config.microsandbox_socket_dir(),
            base_preparation_lock: Arc::new(Mutex::new(())),
        }
    }

    async fn create_sandbox(
        &self,
        id: String,
        options: SandboxOptions,
        pull_policy: PullPolicy,
    ) -> Result<(), IsolationError> {
        tracing::info!(sandbox_id = %id, network_enabled = options.network_enabled, cpus = CAPSULE_CPUS, memory_mib = CAPSULE_MEMORY_MIB, "creating sandbox");
        let result = match options.snapshot_path {
            Some(snapshot_path) => {
                let mut restore = Sandbox::restore(snapshot_path)
                    .name(&id)
                    .cpus(CAPSULE_CPUS)
                    .memory(CAPSULE_MEMORY_MIB);
                if !options.network_enabled {
                    restore = restore.disable_network();
                }
                for mount in options.mounts {
                    restore =
                        restore.volume(mount.guest_path, |volume| volume.bind(mount.host_path));
                }
                match restore.restore().await {
                    Ok(_) if options.environment.is_empty() => Ok(()),
                    Ok(sandbox) => {
                        // Restore preserves captured env; supply this environment's values for future execs.
                        let mut modification = sandbox.modify().restart();
                        for (key, value) in options.environment {
                            modification = modification.env(key, value);
                        }
                        modification.apply().await.map(|_| ())
                    }
                    Err(error) => Err(error),
                }
            }
            None => {
                let mut builder = Sandbox::builder(&id)
                    .cpus(CAPSULE_CPUS)
                    .memory(CAPSULE_MEMORY_MIB)
                    .image(BASE_IMAGE)
                    .root_disk(CAPSULE_DISK_MIB)
                    .pull_policy(pull_policy);
                if !options.network_enabled {
                    builder = builder.disable_network();
                }
                for mount in options.mounts {
                    builder =
                        builder.volume(mount.guest_path, |volume| volume.bind(mount.host_path));
                }
                for (key, value) in options.environment {
                    builder = builder.env(key, value);
                }
                builder.create_detached().await.map(|_| ())
            }
        };
        if let Err(error) = result {
            if Sandbox::get(&id).await.is_ok()
                && let Err(cleanup_error) = self.remove(id.clone()).await
            {
                tracing::warn!(sandbox_id = %id, %cleanup_error, "could not remove failed sandbox creation");
            }
            return Err(IsolationError::new(error.to_string()));
        }

        tracing::info!(sandbox_id = %id, "sandbox created");

        Ok(())
    }

    /// Remove a sandbox left behind by a build that never finished.
    ///
    /// Setup sandboxes are named after the recipe they build, so a sandbox already
    /// under that name is always a leftover rather than something in use: an
    /// interrupted build, or one whose cleanup never ran because the process was
    /// signalled. Without this, a build that was stopped keeps the next start from
    /// happening at all.
    async fn remove_leftover(&self, id: &str) {
        if Sandbox::get(id).await.is_err() {
            return;
        }
        tracing::info!(sandbox_id = %id, "removing a sandbox left by an unfinished build");
        if let Err(error) = self.remove(id.to_owned()).await {
            tracing::warn!(sandbox_id = %id, %error, "could not remove a leftover sandbox");
        }
    }

    async fn ensure_workspace(&self) -> Result<BaseSandboxSnapshot, IsolationError> {
        self.ensure_snapshot(
            BaseSandboxRecipe::new(
                "workspace",
                self.config.environment_snapshots_dir("workspace"),
                Duration::from_secs(72 * 60 * 60),
                vec![SetupStep::new(
                    "preparing workspace packages",
                    workspace_setup(),
                )],
            ),
            None,
            None,
        )
        .await
    }

    async fn ensure_snapshot(
        &self,
        recipe: BaseSandboxRecipe,
        parent: Option<BaseSandboxSnapshot>,
        progress: Option<&dyn EnvironmentProgressSink>,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        let store = BaseSandboxSnapshotStore::new(&recipe);
        let current = store.current()?;
        // A cached child must be rebuilt when its workspace base is newer.
        let parent_refreshed = parent.as_ref().is_some_and(|parent| {
            current
                .as_ref()
                .is_some_and(|current| parent.created_at_ms > current.created_at_ms)
        });
        if !parent_refreshed && !store.needs_refresh(current.as_ref(), &recipe.setup_command()) {
            return Ok(current.expect("a base snapshot was present"));
        }

        let steps: Vec<&SetupStep> = recipe
            .steps
            .iter()
            .filter(|step| !step.command.trim().is_empty())
            .collect();
        let sandbox_id = format!("altitude-{}-setup", recipe.name);
        let created_at_ms = now_ms();
        tracing::info!(environment = %recipe.name, steps = steps.len(), "building environment base sandbox snapshot");
        self.remove_leftover(&sandbox_id).await;
        let mut options = SandboxOptions::isolated(
            parent.map(|snapshot| snapshot.path.to_string_lossy().into_owned()),
        );
        options.network_enabled = true;
        self.create_sandbox(sandbox_id.clone(), options, PullPolicy::Always)
            .await?;
        let result = async {
            for (completed, step) in steps.iter().enumerate() {
                if let Some(progress) = progress {
                    progress.report(EnvironmentProgress {
                        label: step.label.clone(),
                        completed,
                        total: steps.len(),
                    });
                }
                self.run_setup_step(&sandbox_id, step).await?;
            }
            // Asked while the sandbox is still running, and before the snapshot
            // is taken, because it can only be asked of a live machine.
            let (alpine_release, packages) = self.describe_snapshot(&sandbox_id).await;
            let path = self.suspend(
                sandbox_id.clone(),
                format!("base-{created_at_ms}"),
                recipe.snapshots_dir.clone(),
            ).await?;
            let snapshot = BaseSandboxSnapshot {
                path,
                created_at_ms,
                alpine_release,
                packages,
                setup_command: recipe.setup_command(),
            };
            store.publish(&snapshot)?;
            for stale_snapshot in store.stale_snapshots().await? {
                tracing::info!(snapshot = %stale_snapshot.display(), "removing stale environment base sandbox snapshot");
                if let Err(error) = self.remove_snapshot(stale_snapshot).await {
                    tracing::warn!(%error, "could not remove stale environment base sandbox snapshot");
                }
            }
            Ok(snapshot)
        }
        .await;
        if let Err(error) = &result {
            tracing::warn!(environment = %recipe.name, error = %error, "environment base sandbox setup failed");
            if let Err(remove_error) = self.remove(sandbox_id).await {
                tracing::warn!(environment = %recipe.name, error = %remove_error, "could not remove failed environment setup sandbox");
            }
        }
        let snapshot = result?;
        tracing::info!(environment = %recipe.name, snapshot = %snapshot.path.display(), "environment base sandbox ready");
        Ok(snapshot)
    }

    /// Run one build step, failing fast and reporting what the command printed.
    ///
    /// Steps share the sandbox's filesystem but not its shell, so a step must
    /// stand on its own.
    async fn run_setup_step(
        &self,
        sandbox_id: &str,
        step: &SetupStep,
    ) -> Result<(), IsolationError> {
        let (status, output) = self.run_to_end(sandbox_id, &step.command).await?;
        match status {
            0 => Ok(()),
            code => Err(IsolationError::new(format!(
                "{} exited with code {code}: {}",
                step.label,
                output_tail(&output)
            ))),
        }
    }

    /// Runs a command to completion, returning its exit status and its output.
    ///
    /// Output is kept because some commands are asked questions rather than told
    /// to do things; a build step and a description both run through here.
    async fn run_to_end(
        &self,
        sandbox_id: &str,
        command: &str,
    ) -> Result<(i32, String), IsolationError> {
        let mut session = self
            .run_command(sandbox_id.to_owned(), format!("set -eu\n{command}"))
            .await?;
        let mut output = String::new();
        loop {
            let Some(event) = session.events.recv().await else {
                return Err(IsolationError::new(
                    "a command stopped without an exit status",
                ));
            };
            match event {
                CommandEvent::Output { bytes, .. } => {
                    output.push_str(&String::from_utf8_lossy(&bytes))
                }
                CommandEvent::Exited(status) => return Ok((status, output)),
                CommandEvent::Failed(error) => return Err(IsolationError::new(error)),
            }
        }
    }

    /// What the finished snapshot actually contains, asked of the sandbox itself.
    ///
    /// Recorded rather than inferred: the base image is a rolling tag and the
    /// workspace packages are installed from a command with no versions in it, so
    /// nothing outside the snapshot can say what an operator ended up with.
    ///
    /// Failing to ask is not failing to build. A snapshot that cannot describe
    /// itself is still usable, and the panel says so rather than inventing a
    /// version.
    async fn describe_snapshot(&self, sandbox_id: &str) -> (String, String) {
        let unreadable = || {
            tracing::warn!("the sandbox would not describe itself");
            (String::new(), String::new())
        };
        let command = description_command();
        let Ok((0, output)) = self.run_to_end(sandbox_id, &command).await else {
            return unreadable();
        };
        let mut alpine = String::new();
        let mut packages = String::new();
        for line in output.lines() {
            match line.split_once('\t') {
                Some(("alpine", value)) => alpine = value.trim().to_owned(),
                Some(("packages", value)) => packages = value.trim().to_owned(),
                _ => {}
            }
        }
        if alpine.is_empty() && packages.is_empty() {
            return unreadable();
        }
        (alpine, packages)
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

struct BaseSandboxSnapshotStore {
    snapshots_dir: PathBuf,
    refresh_after: Duration,
}

impl BaseSandboxSnapshotStore {
    fn new(recipe: &BaseSandboxRecipe) -> Self {
        Self::at(recipe.snapshots_dir.clone(), recipe.refresh_after)
    }

    /// The store for an environment's snapshots, by directory.
    ///
    /// A caller that only means to read what a snapshot says does not have a
    /// recipe to hand, and should not need one.
    fn at(snapshots_dir: PathBuf, refresh_after: Duration) -> Self {
        Self {
            snapshots_dir,
            refresh_after,
        }
    }

    fn current(&self) -> Result<Option<BaseSandboxSnapshot>, IsolationError> {
        let manifest = match fs::read_to_string(self.snapshots_dir.join("current")) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(IsolationError::new(error.to_string())),
        };
        // Every field but the last is single-line by nature, and the setup
        // command is last because it is not.
        let mut fields = manifest.splitn(5, '\n');
        let Some(relative_path) = fields.next().filter(|path| !path.is_empty()) else {
            return Err(IsolationError::new(
                "base snapshot manifest has no snapshot path",
            ));
        };
        let Some(created_at_ms) = fields.next() else {
            return Err(IsolationError::new(
                "base snapshot manifest has no creation time",
            ));
        };
        let created_at_ms = created_at_ms.parse().map_err(|_| {
            IsolationError::new("base snapshot manifest has an invalid creation time")
        })?;
        let path = self.snapshots_dir.join(relative_path);
        if !path.exists() {
            return Err(IsolationError::new(format!(
                "base snapshot is missing: {}",
                path.display()
            )));
        }
        Ok(Some(BaseSandboxSnapshot {
            path,
            created_at_ms,
            alpine_release: fields.next().unwrap_or_default().to_owned(),
            packages: fields.next().unwrap_or_default().to_owned(),
            setup_command: fields.next().unwrap_or_default().to_owned(),
        }))
    }

    fn needs_refresh(&self, current: Option<&BaseSandboxSnapshot>, setup_command: &str) -> bool {
        let Some(current) = current else {
            return true;
        };
        current.setup_command != setup_command
            || now_ms().saturating_sub(current.created_at_ms)
                >= self
                    .refresh_after
                    .as_millis()
                    .try_into()
                    .unwrap_or(i64::MAX)
    }

    fn publish(&self, snapshot: &BaseSandboxSnapshot) -> Result<(), IsolationError> {
        fs::create_dir_all(&self.snapshots_dir)
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let relative_path = snapshot
            .path
            .strip_prefix(&self.snapshots_dir)
            .map_err(|_| IsolationError::new("base snapshot is outside its environment store"))?
            .to_string_lossy();
        let manifest_path = self.snapshots_dir.join("current");
        let temporary_path = self
            .snapshots_dir
            .join(format!("current-{}.tmp", process::id()));
        fs::write(
            &temporary_path,
            format!(
                "{relative_path}\n{}\n{}\n{}\n{}",
                snapshot.created_at_ms,
                snapshot.alpine_release,
                snapshot.packages,
                snapshot.setup_command
            ),
        )
        .map_err(|error| IsolationError::new(error.to_string()))?;
        fs::rename(temporary_path, manifest_path)
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    async fn stale_snapshots(&self) -> Result<Vec<PathBuf>, IsolationError> {
        let current = self.current()?.map(|snapshot| snapshot.path);
        let snapshots = Snapshot::list_dir(&self.snapshots_dir)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let mut stale = Vec::new();
        for snapshot in snapshots {
            let path = snapshot
                .path()
                .map_err(|error| IsolationError::new(error.to_string()))?;
            if Some(path) != current.as_deref() {
                stale.push(path.to_owned());
            }
        }
        Ok(stale)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis()
        .try_into()
        .expect("millisecond timestamp does not fit in i64")
}

impl IsolationEngine for MicrosandboxIsolation {
    async fn provision(
        &self,
        id: String,
        task_snapshot: Option<String>,
        development_environment: DevelopmentEnvironment,
        recipes: Vec<EnvironmentRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        environment::provision(
            self,
            &self.config,
            id,
            task_snapshot,
            development_environment,
            recipes,
            progress,
        )
        .await
    }

    async fn prepare(&self, status: &watch::Sender<IsolationStatus>) -> Result<(), IsolationError> {
        private_directory(self.config.data_dir())?;
        private_directory(&self.data_home)?;
        private_directory(&self.cache_home)?;
        tracing::info!("installing Microsandbox runtime");
        status.send_replace(IsolationStatus::InstallingRuntime);
        let socket_safe_home = self.ensure_socket_safe_home()?;
        let backend = LocalBackend::builder()
            .home(socket_safe_home)
            .cache_dir(&self.cache_home)
            .build()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        ensure_runtime(backend.config(), InstallOptions::default())
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        set_default_backend(backend);

        tracing::info!("Microsandbox runtime ready");

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

        tracing::info!("base sandbox image ready");

        status.send_replace(IsolationStatus::PreparingWorkspacePackages);
        let _preparation = self.base_preparation_lock.lock().await;
        self.ensure_workspace().await?;

        Ok(())
    }

    async fn create(&self, id: String, options: SandboxOptions) -> Result<(), IsolationError> {
        self.create_sandbox(id, options, PullPolicy::Never).await
    }

    fn base_description(&self) -> Option<EnvironmentDescription> {
        // The base environment is the workspace base: the same snapshot every
        // other environment is built on top of, so it describes itself from
        // there rather than from a `coding-base` snapshot of its own.
        let store = BaseSandboxSnapshotStore::at(
            self.config.environment_snapshots_dir("workspace"),
            REFRESH_AFTER,
        );
        let snapshot = store.current().ok().flatten()?;
        Some(EnvironmentDescription {
            alpine_release: snapshot.alpine_release,
            packages: snapshot
                .packages
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
            built_at_ms: snapshot.created_at_ms,
        })
    }

    async fn ensure_base(
        &self,
        recipe: BaseSandboxRecipe,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        let _preparation = self.base_preparation_lock.lock().await;
        // The workspace base is app startup's concern and reports on its own
        // screen; a task waits only on the environment built from it.
        let workspace = self.ensure_workspace().await?;
        self.ensure_snapshot(recipe, Some(workspace), Some(progress))
            .await
    }

    async fn run_command(
        &self,
        id: String,
        command: String,
    ) -> Result<CommandSession, IsolationError> {
        tracing::info!(sandbox_id = %id, "starting sandbox command");
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
        let control = process.control();
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
                        Some(Command::Interrupt) => {
                            if let Err(error) = control.signal(2).await {
                                let _ = event_tx.send(CommandEvent::Failed(error.to_string()));
                                break;
                            }
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
        tracing::info!(sandbox_id = %id, "removing sandbox");
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
        tracing::info!(sandbox_id = %id, "suspending sandbox");
        private_directory(&snapshots_dir)?;
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .stop_with_timeout(Duration::from_secs(10))
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        // Altitude keeps one checkpoint per capsule; each capture owns its snapshot group.
        let snapshot = Snapshot::builder(&snapshot_name)
            .group(snapshot_name)
            .from_sandbox(&id)
            .dest_dir(snapshots_dir)
            .create()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .remove()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let path = snapshot
            .path()
            .map_err(|error| IsolationError::new(error.to_string()))?
            .to_owned();
        tracing::info!(sandbox_id = %id, snapshot = %path.display(), "sandbox suspended");
        Ok(path)
    }

    async fn remove_snapshot(&self, path: PathBuf) -> Result<(), IsolationError> {
        // Successors contain their own disk layers; retained ancestry is not our storage policy.
        Snapshot::remove(&path.to_string_lossy(), true)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    async fn guest_regular_file_exists(
        &self,
        id: String,
        path: String,
    ) -> Result<bool, IsolationError> {
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?
            .connect_or_start()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let output = sandbox
            .shell(format!("test -f {}", environment::shell_quote(&path)))
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        match output.status().code {
            0 => Ok(true),
            1 => Ok(false),
            code => Err(IsolationError::new(format!(
                "guest file check exited with code {code}"
            ))),
        }
    }
}

/// The end of a failing command's output, where the reason usually is.
fn output_tail(output: &str) -> &str {
    const LIMIT: usize = 1_000;
    let mut start = output.len().saturating_sub(LIMIT);
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    output[start..].trim()
}
