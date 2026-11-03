use std::{
    collections::HashMap, fs, os::unix::fs::symlink, path::PathBuf, sync::Arc, time::Duration,
};

use microsandbox::{
    LocalBackend, Sandbox,
    sandbox::{MAX_SANDBOX_LIST_LIMIT, PullPolicy, PullProgress, exec::ExecEvent, fs::FsSetAttrs},
    set_default_backend,
    snapshot::Snapshot,
};
use tokio::sync::{mpsc, watch};

use futures_util::{StreamExt, stream};

use crate::project::Project;

use crate::isolation::{
    BaseSandboxRecipe, BaseSandboxSnapshot, CREDENTIAL_PREFIX, Command, CommandEvent,
    CommandSession, CredentialCheck, CredentialReport, CredentialSandbox, EnvironmentDescription,
    EnvironmentProgressSink, FileTally, FileTransferSink, ImagePull, IsolationEngine,
    IsolationError, IsolationStatus, OutputStream, ProfileRecipe, SandboxCapacity, SandboxCeiling,
    SandboxOptions, SandboxPurpose, VsockRoute, WATCHER_PREFIX,
    sandbox::{self, REFRESH_AFTER, private_directory},
};

use super::base::BASE_IMAGE;
use super::base::PREPARATION_SANDBOX;
use super::files::{COPY_CONCURRENCY, host_tree, io_error, permissions};
use super::snapshot::BaseSandboxSnapshotStore;
use super::{MicrosandboxIsolation, connect};

// One impl block: a trait whose methods return `impl Future` cannot be implemented
// in two, so the engine's surface stays in this file and the work stays beside it.
impl IsolationEngine for MicrosandboxIsolation {
    fn sandbox_capacity(&self) -> SandboxCapacity {
        self.host_capacity
            .lock()
            .expect("capacity lock poisoned")
            .sandbox_capacity()
    }

    fn observe_demand(&self, queued_tasks: usize) -> SandboxCapacity {
        let mut capacity = self.host_capacity.lock().expect("capacity lock poisoned");
        capacity.observe(queued_tasks);
        capacity.sandbox_capacity()
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
            host_capacity: Arc::clone(&self.host_capacity),
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
        let recipe = sandbox::composed_recipe(&self.config, project, recipes).ok()?;
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
        // A credential shell lives as long as the terminal in front of it, and a
        // watcher sandbox as long as the host that made it, so one that is still
        // here belongs to a process that is gone.
        self.remove_leftover_sandboxes().await;

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

        status.send_replace(IsolationStatus::PreparingBasePackages);
        let _build = self.build_lock.acquire().await?;
        self.ensure_workspace().await?;

        Ok(())
    }

    async fn create(
        &self,
        id: String,
        mut options: SandboxOptions,
        purpose: SandboxPurpose,
    ) -> Result<(), IsolationError> {
        let ceiling = match purpose {
            SandboxPurpose::Capsule => self.sandbox_ceiling(),
            SandboxPurpose::Command => SandboxCeiling::COMMAND,
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
        recipe: BaseSandboxRecipe,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        let _build = self.build_lock.acquire().await?;
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
        let sandbox = connect(&id).await?;
        let fs = sandbox.fs();
        let tree = self.walk_guest(&fs, &root, &excluded).await?;
        progress.counted(tree.whole);
        let mut moved = FileTally::default();
        for directory in &tree.directories {
            fs::create_dir_all(destination.join(directory)).map_err(io_error)?;
        }
        for file in &tree.files {
            if let Some(parent) = destination.join(&file.relative).parent() {
                fs::create_dir_all(parent).map_err(io_error)?;
            }
        }
        // One round trip per file, several in flight: a checkout is hundreds of small
        // files and waiting for each in turn is what made a transfer look stuck.
        let fs = &fs;
        let planned: Vec<(String, PathBuf, u32, u64)> = tree
            .files
            .iter()
            .map(|file| {
                (
                    file.guest.clone(),
                    destination.join(&file.relative),
                    file.mode,
                    file.size,
                )
            })
            .collect();
        let copying = planned
            .into_iter()
            .map(|(guest, host, mode, size)| async move {
                fs.copy_to_host(&guest, &host)
                    .await
                    .map_err(|error| IsolationError::new(error.to_string()))?;
                // A copy is bytes; what a task left is a tree, so the mode comes too.
                fs::set_permissions(&host, permissions(mode)).map_err(io_error)?;
                progress.moved(FileTally {
                    files: 1,
                    bytes: size,
                });
                Ok::<u64, IsolationError>(size)
            });
        let mut copying = stream::iter(copying).buffer_unordered(COPY_CONCURRENCY);
        while let Some(result) = copying.next().await {
            moved.bytes += result?;
            moved.files += 1;
        }
        for link in &tree.links {
            let host = destination.join(&link.relative);
            if let Some(parent) = host.parent() {
                fs::create_dir_all(parent).map_err(io_error)?;
            }
            let _ = fs::remove_file(&host);
            symlink(&link.target, &host).map_err(io_error)?;
        }
        tracing::info!(
            sandbox_id = %id,
            files = moved.files,
            bytes = moved.bytes,
            "carried a task's files out"
        );
        Ok(moved)
    }

    async fn collect_file(
        &self,
        id: String,
        guest_path: String,
        host_path: PathBuf,
    ) -> Result<u64, IsolationError> {
        let sandbox = connect(&id).await?;
        let fs = sandbox.fs();
        let size = fs
            .stat(&guest_path)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?
            .size;
        if let Some(parent) = host_path.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        fs.copy_to_host(&guest_path, &host_path)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        Ok(size)
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
        fs.copy_from_host(&host_path, &guest_path)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    async fn deliver_files(
        &self,
        id: String,
        source: PathBuf,
        root: String,
        progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        let tree = host_tree(&source)?;
        progress.counted(tree.whole);
        let sandbox = connect(&id).await?;
        let fs = sandbox.fs();
        let guest = |relative: &str| format!("{}/{relative}", root.trim_end_matches('/'));
        let _ = fs.mkdir(&root).await;
        let mut moved = FileTally::default();
        for directory in &tree.directories {
            let _ = fs.mkdir(&guest(directory)).await;
        }
        for file in &tree.files {
            fs.copy_from_host(source.join(&file.relative), &guest(&file.relative))
                .await
                .map_err(|error| IsolationError::new(error.to_string()))?;
            if let Err(error) = fs
                .set_stat(
                    &guest(&file.relative),
                    true,
                    FsSetAttrs {
                        mode: Some(file.mode),
                        ..FsSetAttrs::default()
                    },
                )
                .await
            {
                tracing::warn!(path = %file.relative, %error, "could not restore a file's mode");
            }
            moved.files += 1;
            moved.bytes += file.size;
            progress.moved(FileTally {
                files: 1,
                bytes: file.size,
            });
        }
        for link in &tree.links {
            let _ = fs.remove(&guest(&link.relative)).await;
            fs.symlink(&link.target, &guest(&link.relative))
                .await
                .map_err(|error| IsolationError::new(error.to_string()))?;
        }
        tracing::info!(
            sandbox_id = %id,
            files = moved.files,
            bytes = moved.bytes,
            "put a task's files in"
        );
        Ok(moved)
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
            .map_err(|error| IsolationError::new(error.to_string()))
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
        // Altitude keeps one checkpoint per capsule; each capture owns its snapshot group.
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
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?
            .connect_or_start()
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let output = sandbox
            .shell(format!("test -f {}", sandbox::shell_quote(&path)))
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
