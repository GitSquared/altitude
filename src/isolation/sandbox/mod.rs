use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{config::Config, project::Project};

use super::{
    BaseSandboxRecipe, BaseSandboxSnapshot, CommandEvent, CredentialCheck, CredentialReport,
    CredentialSandbox, EnvironmentProgressSink, IsolationEngine, IsolationError, LayerView,
    OutputStream, PORT_RELAY_GUEST_PORT, PROJECT_CACHE_QUOTA_MIB, STORE_QUOTA_MIB, SandboxMount,
    SandboxOptions, SandboxPurpose, SetupStep, VsockRoute, environment_identity,
};

mod guest;
mod plan;
mod profile;
mod run;

use guest::relative_path;
use plan::{cache_storage_path, expand_guest_path};

pub(crate) use guest::{guest_absolute, run_collecting, shell_quote};
pub(crate) use plan::{
    ProvisionRequest, SandboxKind, SnapshotSource, environment_top_layer, provision,
};
pub use profile::{DefaultFile, GuestPath, PersistentDirectory, ProfileRecipe, SignInCommand};
pub(crate) use run::{
    add_commits_env, create, credential_reports, credential_shell, ensure_project, watcher,
};

pub(super) const GUEST_HOME: &str = "/root";
// Where a project's memory is mounted in every task that runs it. The same path is
// named in the project brief, which is the only thing that tells the agent it exists.
pub(crate) const MEMORY_GUEST_PATH: &str = "~/.altitude/memory";
pub(super) const OPENER: &str = "/usr/local/bin/xdg-open";
pub const REFRESH_AFTER: Duration = Duration::from_secs(72 * 60 * 60);

pub(super) struct SandboxPlan {
    project: Project,
    // Bottom first: what the profiles install, then what the project adds on top
    // of it. Each is one snapshot, and a layer is only a layer when it has
    // something of its own to run.
    layers: Vec<BaseSandboxRecipe>,
    options: SandboxOptions,
    defaults: Vec<(PathBuf, &'static str)>,
    managed_files: Vec<(PathBuf, String)>,
    // Files altitude owns, mounted read-only over their guest paths so the agent
    // reads them and cannot rewrite them. They are separate from `options.mounts`
    // because their guest paths sit inside a writable directory mount, and the
    // provider has to apply the directory before the file on top of it.
    read_only_files: Vec<SandboxMount>,
    local_mounts: Vec<(String, String)>,
}

// The commands are the build: two layers with the same ones are the same layer,
// whichever project asked for it. The host's own environment is part of that, since
// what a marker records is that the commands ran on *this* image.
fn tag_of(steps: &[SetupStep]) -> String {
    let mut layer = steps
        .iter()
        .map(|step| step.command.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(environment) = environment_identity() {
        layer.push('\n');
        layer.push_str(&environment);
    }
    blake3::hash(layer.as_bytes()).to_hex()[..12].to_owned()
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
        // Each contributor to a layer is its own step, so a task waiting on a cold
        // environment can say what it is waiting for. The commands are also what the
        // layer is, and so what names its snapshot.
        let installs = recipes
            .iter()
            .map(|recipe| SetupStep::new(recipe.label, recipe.setup_command))
            .collect::<Vec<_>>();
        let mut layers = Vec::new();
        if !installs.iter().all(|step| step.command.trim().is_empty()) {
            layers.push(BaseSandboxRecipe::new(
                "installs",
                config.installs_snapshots_dir().join(tag_of(&installs)),
                REFRESH_AFTER,
                installs,
            ));
        }
        let project_steps = vec![SetupStep::new(
            format!("preparing the {project_name} environment"),
            project_setup,
        )];
        if !project_steps
            .iter()
            .all(|step| step.command.trim().is_empty())
        {
            layers.push(BaseSandboxRecipe::new(
                format!("coding-{project_name}"),
                // A project id is not a build: a sign-in's narrower composition and
                // the one tasks run on are two builds, and a store shared between
                // them would have each replace the other's snapshot.
                config
                    .project_snapshots_dir(&project_id)
                    .join(tag_of(&project_steps)),
                REFRESH_AFTER,
                project_steps,
            ));
        }
        let commits = project.commits.clone();
        let mut environment = vec![
            ("HOME".into(), GUEST_HOME.into()),
            ("SHELL".into(), "/bin/bash".into()),
            // So that a tool which asks a browser to open something uses the
            // opener altitude answers, not a browser that is not there.
            ("BROWSER".into(), OPENER.into()),
        ];
        if let Some(commits) = &commits {
            add_commits_env(&mut environment, commits);
        }
        let mut plan = Self {
            project,
            layers,
            options: SandboxOptions {
                snapshot_path: None,
                network_enabled: true,
                mounts: Vec::new(),
                environment,
                vsock: Vec::new(),
            },
            defaults: Vec::new(),
            managed_files: Vec::new(),
            read_only_files: Vec::new(),
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
                let readme = include_str!("../assets/mount.md")
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
                    quota_mib: STORE_QUOTA_MIB,
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
            let readme = include_str!("../assets/cache.md")
                .replace("{{guest_path}}", &guest_path.to_string_lossy())
                .replace("{{host_path}}", &host_path.to_string_lossy())
                .replace(
                    "{{instructions}}",
                    "altitude created it for this project and will not put anything \
                     else here.",
                );
            plan.managed_files
                .push((host_path.join("README.md"), readme));
            plan.options.mounts.push(SandboxMount {
                host_path,
                guest_path: guest_path.to_string_lossy().into_owned(),
                readonly: false,
                // Build output, which a build has to be able to run: `target` holds
                // the scripts and binaries a compiler produced. Refusing to execute
                // here stops the tool and protects nothing — the sandbox can copy a
                // file out and run it, so the operator's cache is theirs to share.
                noexec: false,
                quota_mib: PROJECT_CACHE_QUOTA_MIB,
            });
        }
        // Every project a task runs in has memory, whether it asked for it or not:
        // what one task learns has to reach the next, and asking the operator to
        // configure a place for it is asking them to remember to. It sits outside the
        // cache tree, so clearing a cache never costs memory. A profile-only project
        // is a build or a shell, not a place work runs, and gets none.
        if plan.project.id.parse::<uuid::Uuid>().is_ok() {
            let host_path = config.project_memory_dir(&plan.project.id);
            let guest_path = expand_guest_path(MEMORY_GUEST_PATH)?;
            for existing in &plan.options.mounts {
                let existing_guest = Path::new(&existing.guest_path);
                if guest_path.starts_with(existing_guest)
                    || existing_guest.starts_with(&guest_path)
                    || host_path.starts_with(&existing.host_path)
                    || existing.host_path.starts_with(&host_path)
                {
                    return Err(IsolationError::new("project memory overlaps another mount"));
                }
            }
            // The folder travels but its README does not: each machine writes its
            // own from the project it holds, so the note names the project even when
            // the sandbox and the host directory are named only by an id. It points
            // at the brief rather than repeating it, so the project's context has one
            // source and the agent reads it once.
            let readme = include_str!("../assets/memory.md")
                .replace("{{project}}", &plan.project.name)
                .replace("{{guest_path}}", &guest_path.to_string_lossy())
                .replace("{{host_path}}", &host_path.to_string_lossy());
            plan.managed_files
                .push((host_path.join("README.md"), readme));
            // How to wield the indexer on this folder. altitude refreshes both notes,
            // so an edit to them is lost rather than kept: the memory is the agent's,
            // the instructions are the machine's.
            plan.managed_files.push((
                host_path.join("QMD.md"),
                include_str!("../assets/qmd.md").to_owned(),
            ));
            plan.options.mounts.push(SandboxMount {
                host_path,
                guest_path: guest_path.to_string_lossy().into_owned(),
                readonly: false,
                // Markdown to read and edit, never a program to run.
                noexec: true,
                quota_mib: STORE_QUOTA_MIB,
            });
            // The project's brief and altitude's rules are files too. They live on
            // the machine rather than in the guest so they can be mounted read-only
            // like every other note altitude owns, and the agent can reread them.
            let checkout = plan
                .project
                .checkout_path()
                .map(|path| expand_guest_path(&path))
                .transpose()?
                .map(|path| path.to_string_lossy().into_owned());
            let brief_dir = config.project_brief_dir(&plan.project.id);
            for (name, contents) in [
                ("project.md", plan.project.brief(checkout.as_deref())),
                (
                    "harness.md",
                    include_str!("../assets/harness.md").to_owned(),
                ),
            ] {
                let host_path = brief_dir.join(name);
                plan.managed_files.push((host_path.clone(), contents));
                plan.read_only_files.push(SandboxMount {
                    host_path,
                    guest_path: format!("/altitude/{name}"),
                    readonly: true,
                    noexec: true,
                    quota_mib: STORE_QUOTA_MIB,
                });
            }
        }
        // altitude's own files are read-only to the agent: each one is mounted over
        // its guest path, after the directory that holds it, so the notes and
        // guides cannot be rewritten from inside the sandbox.
        let mut read_only = read_only_managed_files(&plan);
        read_only.append(&mut plan.read_only_files);
        plan.read_only_files = read_only;
        Ok(plan)
    }

    fn initialize(&mut self, config: &Config) -> Result<(), IsolationError> {
        private_directory(config.data_dir())?;
        // The directories first: a managed file is written into one, and a file
        // mount's source is written below before it is resolved.
        for mount in &self.options.mounts {
            private_directory(&mount.host_path)?;
        }
        for (path, contents) in &self.managed_files {
            if let Some(parent) = path.parent() {
                private_directory(parent)?;
            }
            // Replace altitude's own file without following a guest-created symlink.
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
        // The overmounts go last, now that the files they expose are written, and
        // every host path is resolved once the whole set exists.
        self.options.mounts.append(&mut self.read_only_files);
        for mount in &mut self.options.mounts {
            mount.host_path = fs::canonicalize(&mount.host_path).map_err(io_error)?;
        }
        Ok(())
    }

    // Taken after `initialize`, which is what makes the mount sources exist and
    // settles their paths.
    fn layer_view(&self) -> LayerView {
        LayerView {
            mounts: self.options.mounts.clone(),
            environment: self.options.environment.clone(),
        }
    }
}

// Each managed file is altitude's own, written to the host and mounted into the
// sandbox. Mounting it again over its guest path, read-only, is what keeps the
// agent from editing it: the directory mount is writable, the file on top is not.
fn read_only_managed_files(plan: &SandboxPlan) -> Vec<SandboxMount> {
    let mut mounts = Vec::new();
    for (path, _) in &plan.managed_files {
        let Some(directory) = plan
            .options
            .mounts
            .iter()
            .filter(|mount| path.starts_with(&mount.host_path))
            .max_by_key(|mount| mount.host_path.as_os_str().len())
        else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(&directory.host_path) else {
            continue;
        };
        mounts.push(SandboxMount {
            host_path: path.clone(),
            guest_path: Path::new(&directory.guest_path)
                .join(relative)
                .to_string_lossy()
                .into_owned(),
            readonly: true,
            noexec: directory.noexec,
            quota_mib: STORE_QUOTA_MIB,
        });
    }
    mounts
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

pub(super) fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
}
