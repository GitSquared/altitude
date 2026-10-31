//! Composing a project into a sandbox: the profiles it mounts, the caches it shares,
//! and the brief and checkout a task starts with.
//!
//! The isolation engine calls this for every sandbox it makes. Nothing here talks to a
//! runtime, and nothing here decides when a sandbox exists.

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use tokio::time;

use crate::{
    config::Config,
    project::{Project, ProjectCommits},
};

use super::{
    BaseSandboxRecipe, BaseSandboxSnapshot, CommandEvent, CredentialCheck, CredentialReport,
    CredentialSandbox, EnvironmentProgressSink, IsolationEngine, IsolationError, OutputStream,
    PORT_RELAY_GUEST_PORT, SandboxMount, SandboxOptions, SandboxSize, SetupStep, VsockRoute,
};

const GUEST_HOME: &str = "/root";
const OPENER: &str = "/usr/local/bin/xdg-open";
pub const REFRESH_AFTER: Duration = Duration::from_secs(72 * 60 * 60);

/// What a credential shell runs to sign a profile in.
///
/// It replaces the login shell the sandbox would otherwise open, so the operator
/// lands in the CLI's own sign-in rather than at a prompt to type it into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignInCommand {
    /// A command that signs in and exits on its own, so the terminal lasts exactly
    /// as long as the sign-in does.
    Direct(&'static str),
    /// A CLI whose own interface has to be driven, so the operator is told what to
    /// do there before the terminal opens.
    Interactive(&'static str),
}

impl SignInCommand {
    pub fn command(self) -> &'static str {
        match self {
            Self::Direct(command) | Self::Interactive(command) => command,
        }
    }

    /// Whether the sign-in is a screen the operator drives rather than a command
    /// that finishes on its own.
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Interactive(_))
    }
}

#[derive(Clone)]
pub struct ProfileRecipe {
    pub profile: String,
    /// What this profile is called where an operator reads it, which is the name
    /// the settings list shows and the shell that sets it up names.
    pub display: &'static str,
    pub sign_in: &'static str,
    pub sign_in_command: SignInCommand,
    /// A command that exits 0 when this profile is signed in, and prints what it
    /// found. Run in a sandbox with the profile mounted, so it sees the login a task
    /// would see and never its contents: what it prints is what an operator reads.
    pub status_command: &'static str,
    pub label: &'static str,
    pub setup_command: &'static str,
    pub directories: Vec<PersistentDirectory>,
    pub environment: Vec<(&'static str, &'static str)>,
}

#[derive(Clone)]
pub enum GuestPath {
    Absolute(&'static str),
    HomeRelative(&'static str),
}

#[derive(Clone)]
pub struct PersistentDirectory {
    pub relative_path: &'static str,
    pub guest_path: GuestPath,
    pub defaults: Vec<DefaultFile>,
    /// Files Altitude owns and refreshes on every provisioning, unlike a default
    /// the operator may have edited.
    pub managed: Vec<(&'static str, &'static str)>,
    pub instructions: &'static str,
    pub task_local_subdirectories: Vec<&'static str>,
}

impl PersistentDirectory {
    pub fn at(
        relative_path: &'static str,
        guest_path: GuestPath,
        instructions: &'static str,
    ) -> Self {
        Self {
            relative_path,
            guest_path,
            defaults: Vec::new(),
            managed: Vec::new(),
            instructions,
            task_local_subdirectories: Vec::new(),
        }
    }

    pub fn root(
        guest_path: GuestPath,
        instructions: &'static str,
        defaults: Vec<DefaultFile>,
    ) -> Self {
        Self {
            relative_path: "",
            guest_path,
            defaults,
            managed: Vec::new(),
            instructions,
            task_local_subdirectories: Vec::new(),
        }
    }

    pub fn with_task_local_subdirectories(mut self, paths: &[&'static str]) -> Self {
        self.task_local_subdirectories = paths.to_vec();
        self
    }

    pub fn with_managed_file(mut self, path: &'static str, contents: &'static str) -> Self {
        self.managed.push((path, contents));
        self
    }
}

#[derive(Clone)]
pub struct DefaultFile {
    pub path: &'static str,
    pub contents: &'static str,
}

struct SandboxPlan {
    project: Project,
    recipe: BaseSandboxRecipe,
    options: SandboxOptions,
    defaults: Vec<(PathBuf, &'static str)>,
    managed_files: Vec<(PathBuf, String)>,
    /// Subdirectories that must not reach the shared profile, as the path they
    /// have under whichever root the sandbox gives them, and where a task sees
    /// them.
    local_mounts: Vec<(String, String)>,
}

impl SandboxPlan {
    fn compose(
        config: &Config,
        project: Project,
        recipes: Vec<ProfileRecipe>,
    ) -> Result<Self, IsolationError> {
        project
            .validate()
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let project_name = project.name.clone();
        let project_id = project.id.clone();
        let project_setup = project.setup_command.clone();
        // Each contributor to the build is its own step, so a task waiting on a cold
        // environment can say what it is waiting for. The commands are also what the
        // build is, and so what names its snapshot.
        let mut steps = vec![SetupStep::new(
            format!("preparing the {project_name} environment"),
            project_setup,
        )];
        for recipe in &recipes {
            steps.push(SetupStep::new(recipe.label, recipe.setup_command));
        }
        let identity = steps
            .iter()
            .map(|step| step.command.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let tag = blake3::hash(identity.as_bytes()).to_hex()[..12].to_owned();
        let commits = project.commits.clone();
        let mut environment = vec![
            ("HOME".into(), GUEST_HOME.into()),
            ("SHELL".into(), "/bin/bash".into()),
            // So that a tool which asks a browser to open something uses the
            // opener Altitude answers, not a browser that is not there.
            ("BROWSER".into(), OPENER.into()),
        ];
        if let Some(commits) = &commits {
            add_commits_env(&mut environment, commits);
        }
        let mut plan = Self {
            project,
            recipe: BaseSandboxRecipe::new(
                format!("coding-{project_name}"),
                // A project id is not a build: a sign-in's narrower composition and
                // the one tasks run on are two builds, and a store shared between
                // them would have each replace the other's snapshot.
                config.project_snapshots_dir(&project_id).join(tag),
                REFRESH_AFTER,
                steps,
            ),
            options: SandboxOptions {
                snapshot_path: None,
                environment_base: None,
                network_enabled: true,
                mounts: Vec::new(),
                environment,
                vsock: Vec::new(),
                size: SandboxSize::CAPSULE,
            },
            defaults: Vec::new(),
            managed_files: Vec::new(),
            local_mounts: Vec::new(),
        };
        let mut profiles = HashSet::new();
        for recipe in recipes {
            relative_path(&recipe.profile, false)?;
            if !profiles.insert(recipe.profile.clone()) {
                return Err(IsolationError::new("duplicate profile recipe"));
            }
            for directory in recipe.directories {
                relative_path(directory.relative_path, true)?;
                let host_path = config
                    .profile_dir(&recipe.profile)
                    .join(directory.relative_path);
                let guest_path = match directory.guest_path {
                    GuestPath::Absolute(path) => {
                        if !path.starts_with('/') {
                            return Err(IsolationError::new("guest mount must be absolute"));
                        }
                        relative_path(&path[1..], false)?;
                        PathBuf::from(path)
                    }
                    GuestPath::HomeRelative(path) => {
                        relative_path(path, false)?;
                        Path::new(GUEST_HOME).join(path)
                    }
                };
                for existing in &plan.options.mounts {
                    let existing_guest = Path::new(&existing.guest_path);
                    if guest_path.starts_with(existing_guest)
                        || existing_guest.starts_with(&guest_path)
                        || host_path.starts_with(&existing.host_path)
                        || existing.host_path.starts_with(&host_path)
                    {
                        return Err(IsolationError::new("persistent mount paths overlap"));
                    }
                }
                let readme = include_str!("assets/mount.md")
                    .replace("{{guest_path}}", &guest_path.to_string_lossy())
                    .replace("{{host_path}}", &host_path.to_string_lossy())
                    .replace("{{instructions}}", directory.instructions);
                plan.managed_files
                    .push((host_path.join("README.md"), readme));
                for (path, contents) in directory.managed {
                    relative_path(path, false)?;
                    plan.managed_files
                        .push((host_path.join(path), contents.to_owned()));
                }
                for path in directory.task_local_subdirectories {
                    relative_path(path, false)?;
                    let source = Path::new(&recipe.profile)
                        .join(directory.relative_path)
                        .join(path);
                    let target = guest_path.join(path);
                    plan.local_mounts.push((
                        source.to_string_lossy().into_owned(),
                        target.to_string_lossy().into_owned(),
                    ));
                }
                for default in directory.defaults {
                    relative_path(default.path, false)?;
                    plan.defaults
                        .push((host_path.join(default.path), default.contents));
                }
                plan.options.mounts.push(SandboxMount {
                    host_path,
                    guest_path: guest_path.to_string_lossy().into_owned(),
                    readonly: false,
                    // The agent writes its own sessions and tokens here, and
                    // nothing here is a program to run.
                    noexec: true,
                });
            }
            for (name, value) in recipe.environment {
                if plan
                    .options
                    .environment
                    .iter()
                    .any(|(existing, _)| existing == name)
                {
                    return Err(IsolationError::new(format!(
                        "duplicate environment variable: {name}"
                    )));
                }
                plan.options.environment.push((name.into(), value.into()));
            }
        }
        // Caches are mounts, not build steps: adding or removing one changes what
        // a task finds warm without invalidating a single built snapshot.
        for cache in &plan.project.caches {
            let guest_path = expand_guest_path(cache)?;
            let host_path = config
                .project_cache_dir(&plan.project.id)
                .join(cache_storage_path(cache));
            for existing in &plan.options.mounts {
                let existing_guest = Path::new(&existing.guest_path);
                if guest_path.starts_with(existing_guest)
                    || existing_guest.starts_with(&guest_path)
                    || host_path.starts_with(&existing.host_path)
                    || existing.host_path.starts_with(&host_path)
                {
                    return Err(IsolationError::new(format!(
                        "cache {cache:?} overlaps another mount"
                    )));
                }
            }
            let readme = include_str!("assets/cache.md")
                .replace("{{guest_path}}", &guest_path.to_string_lossy())
                .replace("{{host_path}}", &host_path.to_string_lossy())
                .replace(
                    "{{instructions}}",
                    "Altitude created it for this project and will not put anything \
                     else here.",
                );
            plan.managed_files
                .push((host_path.join("README.md"), readme));
            plan.options.mounts.push(SandboxMount {
                host_path,
                guest_path: guest_path.to_string_lossy().into_owned(),
                readonly: false,
                // A cache is written by one task and read by the next: the place
                // where a file one agent leaves behind could be run by later work.
                noexec: true,
            });
        }
        Ok(plan)
    }

    fn initialize(&mut self, config: &Config) -> Result<(), IsolationError> {
        private_directory(config.data_dir())?;
        for mount in &mut self.options.mounts {
            private_directory(&mount.host_path)?;
            mount.host_path = fs::canonicalize(&mount.host_path).map_err(io_error)?;
        }
        for (path, contents) in &self.managed_files {
            // Replace Altitude's own file without following a guest-created symlink.
            let temporary = path.with_file_name(format!(".managed-{}", uuid::Uuid::new_v4()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(io_error)?;
            file.write_all(contents.as_bytes()).map_err(io_error)?;
            fs::rename(&temporary, path).map_err(io_error)?;
        }
        for (path, contents) in &self.defaults {
            private_directory(path.parent().expect("default has a parent"))?;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(mut file) => file.write_all(contents.as_bytes()).map_err(io_error)?,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if !fs::symlink_metadata(path)
                        .map_err(io_error)?
                        .file_type()
                        .is_file()
                    {
                        return Err(IsolationError::new(
                            "default config path is not a regular file",
                        ));
                    }
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(())
    }
}

/// The opener every sandbox is given: what a browser would do, handed to the
/// terminal Altitude streams instead.
///
/// Written per sandbox rather than built into the environment, because it is
/// Altitude's plumbing and travels with Altitude's code rather than with a
/// snapshot. It goes in through a quoted heredoc, so the script is the file.
fn opener_script() -> String {
    format!(
        "mkdir -p /usr/local/bin\ncat > {OPENER} <<'ALTITUDE_OPENER'\n{}ALTITUDE_OPENER\nchmod 0755 {OPENER}\n",
        include_str!("assets/xdg-open.sh")
    )
}

pub(super) fn relative_path(path: &str, allow_empty: bool) -> Result<(), IsolationError> {
    if (!allow_empty && path.is_empty())
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(IsolationError::new(format!(
            "invalid persistent path: {path}"
        )));
    }
    Ok(())
}

pub(crate) fn private_directory(path: &Path) -> Result<(), IsolationError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_dir()
    {
        return Err(IsolationError::new(format!(
            "not a storage directory: {}",
            path.display()
        )));
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(io_error)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)
}

fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
}

/// Builds a project's snapshot without making a sandbox for a task.
///
/// A machine that has just been sent a configuration builds what that configuration
/// asks for before it can be given work, and this is that build: the same one a task
/// would wait for, done ahead of the task. The snapshot is the project's, shared by
/// every task that runs in it and refreshed on the same clock.
pub async fn ensure_project(
    engine: &impl IsolationEngine,
    config: &Config,
    project: Project,
    recipes: Vec<ProfileRecipe>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<BaseSandboxSnapshot, IsolationError> {
    let mut plan = SandboxPlan::compose(config, project, recipes)?;
    plan.initialize(config)?;
    engine.ensure_base(plan.recipe, progress).await
}

enum SnapshotSource {
    /// A task's capsule: the environment boundary it was exported with is part of
    /// what it is, and a capsule that travels has to prove it.
    Capsule(String),
    /// An environment already built here, taken as it is. A sign-in or a check
    /// reads or writes a login, which is no less true of a day-old image, so no
    /// build is asked about and no boundary is read: that belongs to a capsule.
    Environment(String),
}

/// What a sandbox is for, which decides where its own state goes and whether it
/// records the environment it was built on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SandboxKind {
    Task,
    /// A credential shell's. Its own state goes nowhere, and it is thrown away
    /// where it was made.
    Shell,
}

impl SandboxKind {
    fn state_root(self) -> &'static str {
        match self {
            Self::Task => "/altitude/task-state",
            Self::Shell => "/tmp/altitude-shell",
        }
    }

    /// Whether the environment boundary is recorded, which only a capsule that
    /// travels has to prove.
    fn records_environment(self) -> bool {
        self == Self::Task
    }
}

/// What one provisioning call builds: the sandbox's identity, and what goes in
/// it.
pub(super) struct ProvisionRequest {
    pub id: String,
    pub snapshot: Option<String>,
    pub project: Project,
    pub recipes: Vec<ProfileRecipe>,
    pub vsock: Vec<VsockRoute>,
}

pub(super) async fn provision(
    engine: &impl IsolationEngine,
    config: &Config,
    request: ProvisionRequest,
    progress: &dyn EnvironmentProgressSink,
) -> Result<(), IsolationError> {
    let ProvisionRequest {
        id,
        snapshot,
        project,
        recipes,
        vsock,
    } = request;
    let mut plan = SandboxPlan::compose(config, project, recipes)?;
    plan.options.vsock = vsock;
    create(
        engine,
        config,
        id,
        plan,
        SandboxKind::Task,
        snapshot.map(SnapshotSource::Capsule),
        progress,
    )
    .await
}

/// The build a set of recipes describes.
///
/// What a project *is*: its setup steps are its identity, so this is what says
/// whether one is already built here, and it is also what a sandbox is made from.
pub(super) fn composed_recipe(
    config: &Config,
    project: Project,
    recipes: Vec<ProfileRecipe>,
) -> Result<BaseSandboxRecipe, IsolationError> {
    SandboxPlan::compose(config, project, recipes).map(|plan| plan.recipe)
}

/// What profiles' own checks say about them, run in one sandbox that is thrown away
/// after them.
///
/// The sandbox is the one a credential shell would get, so a report is what opening
/// one would find. It is made from the environment's snapshot as it is rather than
/// from a build: a check reads a login, and an environment a few days past its
/// refresh can still answer. Nothing of the check is kept: each command prints what
/// it found, and the operator reads that.
pub(super) async fn credential_reports(
    engine: &impl IsolationEngine,
    config: &Config,
    sandbox: CredentialSandbox,
    checks: Vec<CredentialCheck>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<Option<Vec<(String, CredentialReport)>>, IsolationError> {
    let CredentialSandbox {
        id,
        recipes,
        snapshot,
        vsock: _,
    } = sandbox;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let mut plan = SandboxPlan::compose(config, Project::profile_build(), recipes)?;
    // One command and nothing else, so it is sized like the provider watcher
    // rather than like a task.
    plan.options.size = SandboxSize::COMMAND;
    create(
        engine,
        config,
        id.clone(),
        plan,
        SandboxKind::Shell,
        Some(SnapshotSource::Environment(snapshot)),
        progress,
    )
    .await?;
    let mut reports = Vec::with_capacity(checks.len());
    for check in checks {
        match run_to_end(engine, id.clone(), check.command).await {
            Ok((status, output)) => reports.push((
                check.profile.clone(),
                CredentialReport {
                    output,
                    signed_in: status == 0,
                },
            )),
            Err(error) => {
                if let Err(error) = engine.remove(id.clone()).await {
                    tracing::warn!(sandbox_id = %id, %error, "could not remove a sandbox after a check");
                }
                return Err(error);
            }
        }
    }
    if let Err(error) = engine.remove(id.clone()).await {
        tracing::warn!(sandbox_id = %id, %error, "could not remove a sandbox after a check");
    }
    Ok(Some(reports))
}

/// A sandbox for setting up one profile's credentials, and the command that opens
/// a shell in it.
///
/// It mounts everything a task mounts, so what is signed in to here is what a task
/// will use, and it is not a task: no capsule, no record, and it is thrown away
/// when the shell ends. The subdirectories a task keeps on its own disk go to a
/// scratch directory instead, because a login shell writes session files and they
/// must not reach the profile every task shares.
///
pub(super) async fn credential_shell(
    engine: &impl IsolationEngine,
    config: &Config,
    sandbox: CredentialSandbox,
    profile: &str,
    progress: &dyn EnvironmentProgressSink,
) -> Result<String, IsolationError> {
    let CredentialSandbox {
        id,
        recipes,
        snapshot,
        vsock,
    } = sandbox;
    let command = recipes
        .iter()
        .find(|recipe| recipe.profile == profile)
        .map(|recipe| recipe.sign_in_command.command())
        .ok_or_else(|| IsolationError::new(format!("no recipe for the {profile} profile")))?;
    let mut plan = SandboxPlan::compose(config, Project::profile_build(), recipes)?;
    let relay = vsock
        .iter()
        .any(|route| route.guest_port == PORT_RELAY_GUEST_PORT);
    plan.options.vsock = vsock;
    create(
        engine,
        config,
        id,
        plan,
        SandboxKind::Shell,
        snapshot.map(SnapshotSource::Environment),
        progress,
    )
    .await?;
    if !relay {
        return Ok(command.to_owned());
    }
    Ok(super::with_helper(command))
}

/// A sandbox that answers a provider's read commands with an owner's profile
/// mounted.
///
/// The shell shape is deliberate: it mounts the profile a task would mount and
/// keeps its own state out of it, but it is not a task, so it has no capsule and
/// nothing travels from it. The caller scopes the config to one owner first, so
/// the profile it mounts is that owner's alone.
pub(super) async fn watcher(
    engine: &impl IsolationEngine,
    config: &Config,
    id: String,
    project: Project,
    recipes: Vec<ProfileRecipe>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<(), IsolationError> {
    let mut plan = SandboxPlan::compose(config, project, recipes)?;
    // This sandbox runs a provider's read command and nothing else, so it is
    // sized for one command rather than for an agent.
    plan.options.size = SandboxSize::COMMAND;
    create(engine, config, id, plan, SandboxKind::Shell, None, progress).await
}

/// Creates a sandbox shaped the way Altitude expects, and binds the subdirectories
/// that must not reach the shared profile.
async fn create(
    engine: &impl IsolationEngine,
    config: &Config,
    id: String,
    mut plan: SandboxPlan,
    kind: SandboxKind,
    snapshot: Option<SnapshotSource>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<(), IsolationError> {
    plan.initialize(config)?;
    let (path, base) = match snapshot {
        Some(SnapshotSource::Capsule(path)) => {
            let base = super::capsule::EnvironmentBase::from_capsule(&path)
                .await
                .map_err(IsolationError::new)?;
            (path, Some(base))
        }
        Some(SnapshotSource::Environment(path)) => (path, None),
        None => {
            let path = engine
                .ensure_base(plan.recipe, progress)
                .await?
                .path
                .to_string_lossy()
                .into_owned();
            let base = match kind.records_environment() {
                true => Some(
                    super::capsule::EnvironmentBase::from_environment(&path)
                        .await
                        .map_err(IsolationError::new)?,
                ),
                false => None,
            };
            (path, base)
        }
    };
    let relay = plan
        .options
        .vsock
        .iter()
        .any(|route| route.guest_port == PORT_RELAY_GUEST_PORT);
    plan.options.snapshot_path = Some(path);
    plan.options.environment_base = base;
    engine.create(id.clone(), plan.options).await?;
    let mut command = String::from("set -eu\n");
    command.push_str(&opener_script());
    if relay {
        command.push_str(&super::install_script());
    }
    let root = kind.state_root();
    for (source, target) in plan.local_mounts {
        let source = shell_quote(&Path::new(root).join(source).to_string_lossy());
        let target = shell_quote(&target);
        command.push_str(&format!(
            "mkdir -p {source} {target}\nmount --bind {source} {target}\n"
        ));
    }
    if kind == SandboxKind::Task {
        command.push_str(&project_scripts(&plan.project)?);
    }
    if let Err(error) = run_setup(engine, id.clone(), command).await {
        if let Err(cleanup_error) = engine.remove(id).await {
            tracing::warn!(%cleanup_error, "could not remove sandbox after its directory setup failed");
        }
        return Err(error);
    }
    Ok(())
}

/// Put a project's commit identity into the environment every git command in the
/// sandbox reads.
///
/// Git applies `GIT_CONFIG_*` pairs on top of the profile's global config, so a
/// project overrides the name and email while the signing key and its format stay
/// with the profile that holds the key.
fn add_commits_env(environment: &mut Vec<(String, String)>, commits: &ProjectCommits) {
    let name = commits.name.trim();
    let email = commits.email.trim();
    if name.is_empty() || email.is_empty() {
        return;
    }
    for variable in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
        environment.push((variable.to_owned(), name.to_owned()));
    }
    for variable in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
        environment.push((variable.to_owned(), email.to_owned()));
    }
    let mut pairs = vec![("user.name", name), ("user.email", email)];
    if let Some(key) = commits
        .signing_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        pairs.push(("user.signingkey", key));
        pairs.push(("commit.gpgsign", "true"));
    } else {
        // A project that names its own identity and no key does not inherit the
        // login's signing setting by accident.
        pairs.push(("commit.gpgsign", "false"));
    }
    environment.push(("GIT_CONFIG_COUNT".to_owned(), pairs.len().to_string()));
    for (index, (key, value)) in pairs.into_iter().enumerate() {
        environment.push((format!("GIT_CONFIG_KEY_{index}"), key.to_owned()));
        environment.push((format!("GIT_CONFIG_VALUE_{index}"), value.to_owned()));
    }
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Where a project's paths are inside the sandbox.
///
/// A project writes `~/` because that is how it reads on the host; the sandbox's
/// home is the guest home, and an absolute path is already the guest's.
fn expand_guest_path(path: &str) -> Result<PathBuf, IsolationError> {
    let path = path.trim().trim_end_matches('/');
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => Path::new(GUEST_HOME).join(rest),
        None if path.starts_with('/') => PathBuf::from(path),
        None => {
            return Err(IsolationError::new(format!(
                "project path {path:?} must be absolute or start with `~/`"
            )));
        }
    };
    relative_path(expanded.to_string_lossy().trim_start_matches('/'), false)?;
    Ok(expanded)
}

/// Where a cache's host directory lives under the project's cache root.
///
/// The path is mirrored rather than hashed so a directory listing says what it
/// holds: `home/.cargo` is the sandbox's `~/.cargo`.
fn cache_storage_path(cache: &str) -> PathBuf {
    let cache = cache.trim().trim_end_matches('/');
    match cache.strip_prefix("~/") {
        Some(rest) => Path::new("home").join(rest),
        None => Path::new("abs").join(cache.trim_start_matches('/')),
    }
}

/// What a task's sandbox is told about its project, and the checkout it starts from.
///
/// The brief is written into the capsule rather than mounted, because it is
/// Altitude's note to the agent and not the operator's file. The clone is
/// guarded on the checkout, so resuming a paused capsule or landing a transferred
/// one leaves the work already there alone.
fn project_scripts(project: &Project) -> Result<String, IsolationError> {
    let checkout = project
        .checkout_path()
        .map(|path| expand_guest_path(&path))
        .transpose()?
        .map(|path| path.to_string_lossy().into_owned());
    let mut script = String::from("mkdir -p /altitude\n");
    script.push_str(&format!(
        "printf '%s' {} > /altitude/project.md\n",
        shell_quote(&project.brief(checkout.as_deref()))
    ));
    // What Altitude itself does, as opposed to what this project is: the same file
    // for every task, so a project brief never has to carry harness knowledge and
    // harness knowledge never drifts per project.
    script.push_str(&format!(
        "printf '%s' {} > /altitude/harness.md\n",
        shell_quote(include_str!("assets/harness.md"))
    ));
    if let (Some(repository), Some(checkout)) = (&project.repository, &checkout) {
        let parent = Path::new(checkout)
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_else(|| GUEST_HOME.to_owned());
        // No emptiness test: a cache mounted inside the checkout —
        // `~/altitude/target`, say — has already made the directory, and a shell
        // asked whether a directory is empty answers "yes" both when it is and
        // when it may not read it, while git, a line later, disagrees. What the
        // question stood for is whether git has started here, which git answers
        // itself: where it has, this is a task resuming into its own tree and is
        // left alone.
        script.push_str(&format!(
            r#"mkdir -p {parent} {checkout}
if ! git -C {checkout} rev-parse --git-dir >/dev/null 2>&1; then
  git -C {checkout} init -q
  if ! git -C {checkout} remote add origin {repository} \
    || ! git -C {checkout} fetch -q origin; then
    echo "altitude: the project's repository could not be fetched" >&2
    exit 1
  fi
  git -C {checkout} remote set-head origin --auto >/dev/null 2>&1 || true
  branch=$(git -C {checkout} symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || echo origin/main)
  if ! git -C {checkout} checkout -q -f -B "${{branch#origin/}}" "$branch"; then
    echo "altitude: the project's repository could not be checked out" >&2
    exit 1
  fi
fi
"#,
            checkout = shell_quote(checkout),
            parent = shell_quote(&parent),
            repository = shell_quote(repository),
        ));
    }
    Ok(script)
}

/// Runs a command to completion, answering with its exit status and everything it
/// printed. A check is a command that answers a question, so its output is the point.
async fn run_to_end(
    engine: &impl IsolationEngine,
    id: String,
    command: String,
) -> Result<(i32, String), IsolationError> {
    let (status, output, errors) = run_collecting(engine, id, command).await?;
    Ok((status, output + &errors))
}

/// Runs a command to completion, answering with its exit status and what it
/// printed on each stream.
///
/// The streams are kept apart because a provider's answer is JSON on one and its
/// warnings are prose on the other, and a caller reading one must not have to
/// parse the other.
pub(crate) async fn run_collecting(
    engine: &impl IsolationEngine,
    id: String,
    command: String,
) -> Result<(i32, String, String), IsolationError> {
    let mut session = engine.run_command(id, command).await?;
    let mut output = String::new();
    let mut errors = String::new();
    while let Some(event) = session.events.recv().await {
        match event {
            CommandEvent::Output { stream, bytes } => {
                let text = String::from_utf8_lossy(&bytes);
                match stream {
                    OutputStream::Stdout => output.push_str(&text),
                    OutputStream::Stderr => errors.push_str(&text),
                }
            }
            CommandEvent::Exited(status) => return Ok((status, output, errors)),
            CommandEvent::Failed(error) => return Err(IsolationError::new(error)),
        }
    }
    Err(IsolationError::new(
        "a command closed without an exit status",
    ))
}

/// How long a sandbox's own directory setup may take before it counts as stuck.
///
/// A command waiting on something that will never come looks exactly like a slow
/// one from here, and a task still being prepared holds its slot while it waits.
/// The clone this runs is the slow part, so the limit is generous: it is here to
/// end a wait, not to hurry a large repository.
const SETUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

async fn run_setup(
    engine: &impl IsolationEngine,
    id: String,
    command: String,
) -> Result<(), IsolationError> {
    // Kept outside the command so a wait that is cut short still says what the
    // sandbox had printed: a prompt is the only trace a stuck setup leaves.
    let mut output = String::new();
    let setup = async {
        let mut session = engine.run_command(id, command).await?;
        while let Some(event) = session.events.recv().await {
            match event {
                CommandEvent::Output { bytes, .. } => {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                }
                CommandEvent::Exited(0) => return Ok(()),
                CommandEvent::Exited(code) => {
                    return Err(IsolationError::new(format!(
                        "task directory setup exited with code {code}: {}",
                        printed_tail(&output),
                    )));
                }
                CommandEvent::Failed(error) => return Err(IsolationError::new(error)),
            }
        }
        Err(IsolationError::new(
            "task directory setup closed without an exit status",
        ))
    };
    let result = time::timeout(SETUP_TIMEOUT, setup).await;
    match result {
        Ok(result) => result,
        Err(_) => Err(IsolationError::new(format!(
            "task directory setup did not finish within {} minutes: {}",
            SETUP_TIMEOUT.as_secs() / 60,
            printed_tail(&output),
        ))),
    }
}

fn printed_tail(output: &str) -> &str {
    const LIMIT: usize = 400;
    match output.char_indices().rev().nth(LIMIT) {
        Some((index, _)) => output[index..].trim(),
        None => output.trim(),
    }
}
