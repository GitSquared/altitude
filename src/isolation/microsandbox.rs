use std::{
    fs,
    io::{self, ErrorKind},
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use microsandbox::{
    LocalBackend, Sandbox,
    sandbox::{MAX_SANDBOX_LIST_LIMIT, PullPolicy, exec::ExecEvent},
    set_default_backend,
    setup::{
        InstallOptions, RuntimeOrigin, Version, ensure_runtime, install_runtime,
        resolve_runtime_version,
    },
    snapshot::Snapshot,
};
use tokio::{
    process::Command as TokioCommand,
    sync::{mpsc, watch},
    task::spawn_blocking,
};

use crate::agent::mcp::AGENT_CHANNEL_PORT;
use crate::config::Config;
use crate::project::Project;

use super::{
    BaseSandboxRecipe, BaseSandboxSnapshot, CREDENTIAL_PREFIX, Command, CommandEvent,
    CommandSession, CredentialCheck, CredentialReport, CredentialSandbox, EnvironmentDescription,
    EnvironmentProgress, EnvironmentProgressSink, IsolationEngine, IsolationError, IsolationStatus,
    OutputStream, ProfileRecipe, SandboxOptions, SetupStep, VsockRoute, WATCHER_PREFIX,
    mount_git_cache,
    sandbox::{self, REFRESH_AFTER, private_directory},
    trust_host_roots,
};

const BASE_IMAGE: &str = "alpine";

/// How large a capsule's writable layer may grow.
///
/// A ceiling rather than a reservation, like the rest of a sandbox's size. It
/// stays out of [`super::SandboxSize`] because it applies only when a sandbox is made
/// from the image: one restored from a snapshot keeps what the snapshot was
/// built with.
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
    /// Microsandbox's Unix sockets exceed macOS path limits under the longer data home.
    socket_safe_home: PathBuf,
    build_lock: BuildLock,
}

/// One environment build at a time on this machine.
///
/// A setup sandbox is named after the store it builds into, so a second build of
/// one environment would tear down the first one's sandbox:
/// [`MicrosandboxIsolation::remove_leftover`]
/// cannot tell a leftover from a build that is running beside it. A mutex would
/// only hold within one process, while the sandbox names and the snapshot store
/// belong to the machine, so the lock is the file itself. Holding it is what
/// makes "a sandbox under that name is a leftover" true.
#[derive(Clone)]
struct BuildLock {
    path: PathBuf,
}

/// A held build slot, released when the build ends or this process dies.
struct BuildGuard {
    _file: fs::File,
}

impl BuildLock {
    fn new(config: &Config) -> Self {
        Self {
            path: config.data_dir().join("environments").join("build.lock"),
        }
    }

    /// Wait for the build slot, without holding a runtime thread while waiting.
    async fn acquire(&self) -> Result<BuildGuard, IsolationError> {
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

impl MicrosandboxIsolation {
    async fn ensure_compatible_runtime(backend: &LocalBackend) -> Result<(), IsolationError> {
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
            socket_safe_home: config.microsandbox_socket_dir(),
            build_lock: BuildLock::new(config),
        }
    }

    /// Connects the harness bridge to the credential shell's empty MCP endpoint.
    fn credential_shell_channel(&self, id: &str) -> Option<VsockRoute> {
        if !id.starts_with(&format!("{CREDENTIAL_PREFIX}shell-")) {
            return None;
        }
        let host_socket = self.config.agent_channel_dir().join(format!("{id}.sock"));
        host_socket.exists().then_some(VsockRoute {
            host_socket,
            guest_port: AGENT_CHANNEL_PORT,
        })
    }

    async fn create_sandbox(
        &self,
        id: String,
        mut options: SandboxOptions,
        pull_policy: PullPolicy,
    ) -> Result<(), IsolationError> {
        mount_git_cache(&mut options, &self.config.git_cache_dir());
        let base = options
            .environment_base
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| IsolationError::new(e.to_string()))?;
        let trusted = trust_host_roots(&mut options, self.config.data_dir());
        tracing::debug!(sandbox_id = %id, network_enabled = options.network_enabled, cpus = options.size.cpus, memory_mib = options.size.memory_mib, "creating sandbox");
        let result = match options.snapshot_path {
            Some(snapshot_path) => {
                let mut restore = Sandbox::restore(snapshot_path)
                    .name(&id)
                    .cpus(options.size.cpus)
                    .memory(options.size.memory_mib);
                if !options.network_enabled {
                    restore = restore.disable_network();
                }
                for mount in options.mounts {
                    restore =
                        restore.volume(mount.guest_path, |volume| volume.bind(mount.host_path));
                }
                for route in options.vsock {
                    restore = restore.vsock(route.host_socket, route.guest_port);
                }
                match restore.restore().await {
                    Ok(_) if options.environment.is_empty() && base.is_none() => Ok(()),
                    Ok(sandbox) => {
                        // Restore preserves captured env; supply this environment's values for future execs.
                        let mut modification = sandbox.modify().restart();
                        if let Some(base) = &base {
                            modification =
                                modification.label(super::capsule::ENVIRONMENT_LABEL, base);
                        }
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
                    .cpus(options.size.cpus)
                    .memory(options.size.memory_mib)
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
                for route in options.vsock {
                    builder = builder.vsock(route.host_socket, route.guest_port);
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

        if trusted {
            self.append_host_roots(&id).await;
        }

        tracing::debug!(sandbox_id = %id, "sandbox created");

        Ok(())
    }

    /// Puts this machine's trusted roots into the sandbox's own store as well.
    ///
    /// A tool that reads the system store — curl, apk, git — has no variable to be
    /// told with, so the bundle is appended to the store the image already has. Done
    /// at creation rather than at build, because a snapshot can be moved to another
    /// machine and one host's inspection root must not travel with it.
    ///
    /// A failure here is not a failure to create a sandbox: a sandbox without these
    /// roots is the sandbox there was before, and most of what runs in one does not
    /// need them.
    async fn append_host_roots(&self, id: &str) {
        let command = "cat /etc/altitude/host-roots.pem >> /etc/ssl/certs/ca-certificates.crt";
        match self.run_to_end(id, command).await {
            Ok((0, _)) => tracing::debug!(sandbox_id = %id, "sandbox trusts this machine's roots"),
            Ok((code, output)) => tracing::warn!(
                sandbox_id = %id,
                code,
                output = %output_tail(&output),
                "could not add this machine's roots to the sandbox store"
            ),
            Err(error) => {
                tracing::warn!(sandbox_id = %id, %error, "could not reach the sandbox store")
            }
        }
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
        tracing::debug!(sandbox_id = %id, "removing a sandbox left behind");
        if let Err(error) = self.remove(id.to_owned()).await {
            tracing::warn!(sandbox_id = %id, %error, "could not remove a leftover sandbox");
        }
    }

    async fn ensure_workspace(&self) -> Result<BaseSandboxSnapshot, IsolationError> {
        self.ensure_snapshot(
            BaseSandboxRecipe::new(
                "workspace",
                self.config.base_snapshots_dir(),
                Duration::from_secs(72 * 60 * 60),
                vec![SetupStep::new("preparing base packages", workspace_setup())],
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
        let sandbox_id = format!(
            "altitude-{}-{}-setup",
            build_namespace(&recipe.snapshots_dir),
            artifact_name(&recipe.name)
        );
        let created_at_ms = now_ms();
        tracing::debug!(environment = %recipe.name, steps = steps.len(), "building environment base sandbox snapshot");
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
                tracing::debug!(snapshot = %stale_snapshot.display(), "removing stale environment base sandbox snapshot");
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
        tracing::debug!(environment = %recipe.name, snapshot = %snapshot.path.display(), "environment base sandbox ready");
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
        let (status, output) = self
            .run_to_end(sandbox_id, &step.command)
            .await
            .map_err(|error| IsolationError::new(format!("{}: {error}", step.label)))?;
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

/// A short, stable tag for whichever directory a build writes into, so two
/// operators' same-named environments cannot collide on one setup sandbox.
fn build_namespace(snapshots_dir: &Path) -> String {
    blake3::hash(snapshots_dir.to_string_lossy().as_bytes()).to_hex()[..12].to_owned()
}

/// A name the runtime accepts for a sandbox.
///
/// A project's name is written for a person and may hold spaces; an id a runtime
/// keys machines by may not. The readable part stays, because a sandbox left
/// behind is looked for by name.
fn artifact_name(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_dash = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_') {
            slug.push(character);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "project".to_owned()
    } else {
        slug.to_owned()
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
        profile: &'static str,
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
    ) -> Result<Option<Vec<(&'static str, CredentialReport)>>, IsolationError> {
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

        tracing::debug!("base sandbox image ready");

        status.send_replace(IsolationStatus::PreparingBasePackages);
        let _build = self.build_lock.acquire().await?;
        self.ensure_workspace().await?;

        Ok(())
    }

    async fn create(&self, id: String, mut options: SandboxOptions) -> Result<(), IsolationError> {
        if let Some(route) = self.credential_shell_channel(&id) {
            options.vsock.push(route);
        }
        self.create_sandbox(id, options, PullPolicy::Never).await
    }

    fn base_description(&self) -> Option<EnvironmentDescription> {
        // The base is what every project's build sits on, so it describes itself
        // from there rather than from a snapshot of its own.
        let store = BaseSandboxSnapshotStore::at(self.config.base_snapshots_dir(), REFRESH_AFTER);
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

    async fn remove(&self, id: String) -> Result<(), IsolationError> {
        tracing::debug!(sandbox_id = %id, "removing sandbox");
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
        tracing::debug!(sandbox_id = %id, "suspending sandbox");
        private_directory(&snapshots_dir)?;
        let sandbox = Sandbox::get(&id)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        sandbox
            .stop_with_timeout(Duration::from_secs(10))
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let base = sandbox
            .config()
            .map_err(|e| IsolationError::new(e.to_string()))?
            .spec
            .labels
            .get(super::capsule::ENVIRONMENT_LABEL)
            .cloned();
        // Altitude keeps one checkpoint per capsule; each capture owns its snapshot group.
        let mut capture = Snapshot::builder(&snapshot_name)
            .group(snapshot_name)
            .from_sandbox(&id)
            .dest_dir(snapshots_dir);
        if let Some(base) = base {
            capture = capture.label(super::capsule::ENVIRONMENT_LABEL, base);
        }
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

/// The end of a failing command's output, where the reason usually is.
fn output_tail(output: &str) -> &str {
    const LIMIT: usize = 1_000;
    let mut start = output.len().saturating_sub(LIMIT);
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    output[start..].trim()
}
