use std::{collections::HashMap, fs, path::PathBuf, sync::Arc, time::Duration};

use microsandbox::{
    LocalBackend, Sandbox,
    sandbox::{MAX_SANDBOX_LIST_LIMIT, PullPolicy, PullProgress, exec::ExecEvent, fs::FsEntryKind},
    set_default_backend,
    snapshot::Snapshot,
};
use tokio::sync::{mpsc, watch};

use crate::project::Project;

use crate::isolation::{
    BaseSandboxRecipe, BaseSandboxSnapshot, CREDENTIAL_PREFIX, Command, CommandEvent,
    CommandSession, CredentialCheck, CredentialReport, CredentialSandbox, EnvironmentDescription,
    EnvironmentProgressSink, FileTally, FileTransferSink, ImagePull, IsolationEngine,
    IsolationError, IsolationStatus, OutputStream, PackageInstall, PackageInstallSink, Pressure,
    ProfileRecipe, SandboxCeiling, SandboxOptions, SandboxPurpose, SandboxRoom, VsockRoute,
    WATCHER_PREFIX,
    sandbox::{self, REFRESH_AFTER, private_directory},
};

use super::base::BASE_IMAGE;
use super::base::{PREPARATION_SANDBOX, SETUP_SUFFIX};
use super::files::{carry_in, carry_out, io_error};
use super::snapshot::{BaseSandboxSnapshotStore, forget_abandoned_stores};
use super::{MicrosandboxIsolation, connect, transfer};

// The boot screen's view of the base build's installs: the status it shows is the
// only thing the sink has to say.
struct BasePackages<'a>(&'a watch::Sender<IsolationStatus>);

impl PackageInstallSink for BasePackages<'_> {
    fn installing(&self, install: PackageInstall) {
        self.0
            .send_replace(IsolationStatus::PreparingBasePackages(install));
    }
}

// One impl block: a trait whose methods return `impl Future` cannot be implemented
// in two, so the engine's surface stays in this file and the work stays beside it.
impl IsolationEngine for MicrosandboxIsolation {
    fn pressure(&self) -> Option<Pressure> {
        self.host_pressure
            .lock()
            .expect("pressure lock poisoned")
            .pressure()
    }

    fn observe_pressure(&self) -> Option<Pressure> {
        self.host_pressure
            .lock()
            .expect("pressure lock poisoned")
            .observe()
    }

    fn sandbox_room(&self) -> SandboxRoom {
        self.host_pressure
            .lock()
            .expect("pressure lock poisoned")
            .room()
    }

    fn project_memory_dir(&self, project_id: &str) -> PathBuf {
        self.config.project_memory_dir(project_id)
    }

    fn for_operator(&self, owner: &str) -> Result<Self, IsolationError> {
        if !self.config.serves_operators() {
            return Ok(self.clone());
        }
        let config = self
            .config
            .for_operator(owner)
            .map_err(|error| IsolationError::new(error.to_string()))?;
        Ok(Self {
            config,
            // The runtime and the build slot belong to the machine, not to one
            // operator: a scoped view shares them rather than making a second
            // runtime the socket cannot carry.
            data_home: self.data_home.clone(),
            cache_home: self.cache_home.clone(),
            socket_safe_home: self.socket_safe_home.clone(),
            build_lock: self.build_lock.clone(),
            host_pressure: Arc::clone(&self.host_pressure),
        })
    }

    async fn ensure_project(
        &self,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        sandbox::ensure_project(self, &self.config, project, recipes, progress).await
    }

    async fn provision(
        &self,
        id: String,
        task_snapshot: Option<String>,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        vsock: Vec<VsockRoute>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        sandbox::provision(
            self,
            &self.config,
            sandbox::ProvisionRequest {
                id,
                snapshot: task_snapshot,
                project,
                recipes,
                vsock,
            },
            progress,
        )
        .await
    }

    async fn credential_shell(
        &self,
        sandbox: CredentialSandbox,
        profile: &str,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<String, IsolationError> {
        sandbox::credential_shell(self, &self.config, sandbox, profile, progress).await
    }

    async fn watcher(
        &self,
        id: String,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        sandbox::watcher(self, &self.config, id, project, recipes, progress).await
    }

    fn project_snapshot(&self, project: Project, recipes: Vec<ProfileRecipe>) -> Option<String> {
        let recipe = sandbox::environment_top_layer(&self.config, project, recipes).ok()??;
        let snapshot = BaseSandboxSnapshotStore::new(&recipe)
            .current()
            .ok()
            .flatten()?;
        Some(snapshot.path.to_string_lossy().into_owned())
    }

    async fn credential_reports(
        &self,
        sandbox: CredentialSandbox,
        checks: Vec<CredentialCheck>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<Option<Vec<(String, CredentialReport)>>, IsolationError> {
        sandbox::credential_reports(self, &self.config, sandbox, checks, progress).await
    }

    async fn remove_leftover_sandboxes(&self) {
        let mut cursor: Option<String> = None;
        loop {
            let listed = Sandbox::list_with(|list| match &cursor {
                Some(cursor) => list.cursor(cursor.clone()).limit(MAX_SANDBOX_LIST_LIMIT),
                None => list.limit(MAX_SANDBOX_LIST_LIMIT),
            })
            .await;
            let page = match listed {
                Ok(page) => page,
                Err(error) => {
                    tracing::debug!(%error, "could not list the sandboxes");
                    return;
                }
            };
            for sandbox in page.sandboxes {
                let name = sandbox.name();
                if name.starts_with(CREDENTIAL_PREFIX) {
                    // The channel's socket is bound by the task layer, which is not up yet.
                    let _ = fs::remove_file(
                        self.config.agent_channel_dir().join(format!("{name}.sock")),
                    );
                    self.remove_leftover(name).await;
                } else if name.starts_with(WATCHER_PREFIX) {
                    self.remove_leftover(name).await;
                } else if name.ends_with(SETUP_SUFFIX) {
                    // An environment build that was killed part-way leaves its sandbox
                    // and the disk image inside it, and nothing else would ever look at
                    // a name like that again.
                    self.remove_leftover(name).await;
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return,
            }
        }
    }

    async fn prepare(&self, status: &watch::Sender<IsolationStatus>) -> Result<(), IsolationError> {
        Self::check_host()?;
        private_directory(self.config.data_dir())?;
        private_directory(&self.data_home)?;
        private_directory(&self.cache_home)?;
        tracing::debug!("installing Microsandbox runtime");
        status.send_replace(IsolationStatus::InstallingRuntime);
        let socket_safe_home = self.ensure_socket_safe_home()?;
        let backend = LocalBackend::builder()
            .home(socket_safe_home)
            .cache_dir(&self.cache_home)
            .build()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        Self::ensure_compatible_runtime(&backend).await?;
        set_default_backend(backend);

        tracing::debug!("Microsandbox runtime ready");
        // A credential shell lives as long as the terminal in front of it, a watcher
        // sandbox as long as the host that made it, and a build sandbox only while its
        // build holds the lock, so one that is still here belongs to a process that is
        // gone. The lock is taken to say that of the builds too: a sweep must not take
        // a build's sandbox out from under a build that is still running.
        if let Ok(_building) = self.build_lock.acquire().await {
            self.remove_leftover_sandboxes().await;
            // Under the same lock: an environment store belongs to the build that is
            // using it, and one nothing composes any more belongs to nobody.
            forget_abandoned_stores(&self.config.environment_store_roots(), REFRESH_AFTER).await;
        }

        status.send_replace(IsolationStatus::PreparingBaseImage(ImagePull::default()));
        let (mut pull, creating) = Sandbox::builder(PREPARATION_SANDBOX)
            .image(BASE_IMAGE)
            .pull_policy(PullPolicy::IfMissing)
            .disable_network()
            .ephemeral(true)
            .replace()
            .create_with_pull_progress()
            .map_err(|error| IsolationError::new(error.to_string()))?;
        // The image comes down layer by layer, concurrently, so the total is
        // the bytes of the whole manifest rather than of the last layer seen.
        let reporting = status.clone();
        let reporter = tokio::spawn(async move {
            let mut progress = ImagePull::default();
            let mut layers: HashMap<usize, u64> = HashMap::new();
            while let Some(event) = pull.recv().await {
                match event {
                    PullProgress::Resolved {
                        total_download_bytes,
                        ..
                    } => progress.total_bytes = total_download_bytes.unwrap_or(0),
                    PullProgress::LayerDownloadProgress {
                        layer_index,
                        downloaded_bytes,
                        ..
                    }
                    | PullProgress::LayerDownloadComplete {
                        layer_index,
                        downloaded_bytes,
                        ..
                    } => {
                        layers.insert(layer_index, downloaded_bytes);
                        progress.downloaded_bytes = layers.values().sum();
                    }
                    _ => continue,
                }
                reporting.send_replace(IsolationStatus::PreparingBaseImage(progress));
            }
        });
        let sandbox = creating
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?
            .map_err(|error| IsolationError::new(error.to_string()))?;
        // The pull channel closes with the create task, so this joins the last
        // update before the phase that follows is announced.
        let _ = reporter.await;
        sandbox
            .stop()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;

        tracing::debug!("base sandbox image ready");

        status.send_replace(IsolationStatus::PreparingBasePackages(
            PackageInstall::default(),
        ));
        let _build = self.build_lock.acquire().await?;
        // One sink for this screen only: a task waits on the environment it builds
        // for itself, and the base is the startup's own concern.
        self.ensure_workspace(Some(&BasePackages(status))).await?;

        Ok(())
    }

    async fn create(
        &self,
        id: String,
        mut options: SandboxOptions,
        purpose: SandboxPurpose,
    ) -> Result<(), IsolationError> {
        let ceiling = match purpose {
            SandboxPurpose::Capsule => self.sandbox_room().offered,
            SandboxPurpose::Command => Some(SandboxCeiling::COMMAND),
        };
        if let Some(route) = self.credential_shell_channel(&id) {
            options.vsock.push(route);
        }
        self.create_sandbox(id, options, ceiling, PullPolicy::Never)
            .await
    }

    fn base_description(&self) -> Option<EnvironmentDescription> {
        // The base is what every project's build sits on, so it describes itself
        // from there rather than from a snapshot of its own.
        let store = BaseSandboxSnapshotStore::at(self.config.base_snapshots_dir(), REFRESH_AFTER);
        let snapshot = store.current().ok().flatten()?;
        Some(EnvironmentDescription {
            os_release: snapshot.os_release,
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
        layers: Vec<BaseSandboxRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        let _build = self.build_lock.acquire().await?;
        // The workspace base is app startup's concern and reports on its own
        // screen; a task waits only on the environment built from it.
        let mut built = self.ensure_workspace(None).await?;
        for layer in layers {
            built = self
                .ensure_snapshot(layer, Some(built), Some(progress), None)
                .await?;
        }
        Ok(built)
    }

    async fn run_command(
        &self,
        id: String,
        command: String,
    ) -> Result<CommandSession, IsolationError> {
        tracing::debug!(sandbox_id = %id, "starting sandbox command");
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

    async fn collect_files(
        &self,
        id: String,
        root: String,
        excluded: Vec<String>,
        destination: PathBuf,
        progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        transfer::carry_tree_out(&id, &root, &excluded, &destination, progress).await
    }

    async fn collect_file(
        &self,
        id: String,
        guest_path: String,
        host_path: PathBuf,
    ) -> Result<u64, IsolationError> {
        let sandbox = connect(&id).await?;
        let fs = sandbox.fs();
        if let Some(parent) = host_path.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        carry_out(&fs, &guest_path, &host_path, 0o600)
            .await
            .map_err(|error| IsolationError::new(format!("carrying out {guest_path}: {error}")))
    }

    async fn deliver_file(
        &self,
        id: String,
        host_path: PathBuf,
        guest_path: String,
    ) -> Result<(), IsolationError> {
        let sandbox = connect(&id).await?;
        let fs = sandbox.fs();
        if let Some(parent) = guest_path.rsplit_once('/').map(|(parent, _)| parent)
            && !parent.is_empty()
        {
            let _ = fs.mkdir(parent).await;
        }
        // A link where this file goes would have the write land on whatever it
        // points at, so the name is cleared first: replacing the link costs nothing,
        // and writing through it would edit a file this one has nothing to do with.
        let _ = fs.remove(&guest_path).await;
        carry_in(&fs, &host_path, &guest_path, 0o600)
            .await
            .map_err(|error| IsolationError::new(format!("putting in {guest_path}: {error}")))?;
        Ok(())
    }

    async fn deliver_files(
        &self,
        id: String,
        source: PathBuf,
        root: String,
        progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        transfer::put_tree_in(&id, source, &root, progress).await
    }

    async fn has_sandbox(&self, id: String) -> bool {
        Sandbox::get(&id).await.is_ok()
    }

    async fn remove(&self, id: String) -> Result<(), IsolationError> {
        tracing::debug!(sandbox_id = %id, "removing sandbox");
        let sandbox = match Sandbox::get(&id).await {
            Ok(sandbox) => sandbox,
            // Removing what is not there has already done what it was for. A record
            // that names a sandbox the runtime has forgotten — a restart, a machine
            // that went away — is a sandbox the caller no longer has to worry about,
            // and reporting it would fail a task for being tidier than expected.
            Err(microsandbox::MicrosandboxError::SandboxNotFound(_)) => {
                self.release_ceiling(&id);
                return Ok(());
            }
            Err(error) => return Err(IsolationError::new(error.to_string())),
        };
        // A stop that runs out of patience is not a sandbox that will not go: the
        // removal is what makes it go, and a task must not fail because the runtime
        // was slow to acknowledge a shutdown it no longer needs to wait for.
        if let Err(error) = sandbox.stop_with_timeout(Duration::from_secs(10)).await {
            tracing::warn!(sandbox_id = %id, %error, "sandbox did not stop in time; removing it");
        }
        sandbox
            .remove()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        self.release_ceiling(&id);
        Ok(())
    }

    async fn suspend(
        &self,
        id: String,
        snapshot_name: String,
        snapshots_dir: PathBuf,
    ) -> Result<PathBuf, IsolationError> {
        tracing::debug!(sandbox_id = %id, "suspending sandbox");
        private_directory(&snapshots_dir)?;
        let sandbox = match Sandbox::get(&id).await {
            Ok(sandbox) => sandbox,
            Err(microsandbox::MicrosandboxError::SandboxNotFound(message)) => {
                return Err(IsolationError::missing_sandbox(message));
            }
            Err(error) => return Err(IsolationError::new(error.to_string())),
        };
        sandbox
            .stop_with_timeout(Duration::from_secs(10))
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        // altitude keeps one checkpoint per capsule; each capture owns its snapshot group.
        let capture = Snapshot::builder(&snapshot_name)
            .group(snapshot_name)
            .from_sandbox(&id)
            .dest_dir(snapshots_dir);
        let snapshot = capture
            .create()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .remove()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        // A suspended guest is not using its memory any more, so its promise goes back.
        self.release_ceiling(&id);
        let path = snapshot
            .path()
            .map_err(|error| IsolationError::new(error.to_string()))?
            .to_owned();
        tracing::debug!(sandbox_id = %id, snapshot = %path.display(), "sandbox suspended");
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
        let sandbox = connect(&id).await?;
        // Asked through the filesystem protocol rather than by running `test`: the
        // image is the guest's to break, and a check that needs a working shell
        // answers questions about the image instead of about the file.
        match sandbox.fs().stat_with_follow(&path, true).await {
            Ok(metadata) => Ok(metadata.kind == FsEntryKind::File),
            // A missing path is a failed stat; anything else is a guest that cannot
            // be asked at all, which is worth failing over rather than reporting as
            // an absence.
            Err(microsandbox::MicrosandboxError::SandboxFsOps(error)) => {
                tracing::debug!(%path, %error, "the guest has nothing at that path");
                Ok(false)
            }
            Err(error) => Err(IsolationError::new(error.to_string())),
        }
    }
}
