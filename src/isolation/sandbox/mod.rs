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
    CredentialSandbox, EnvironmentProgressSink, IsolationEngine, IsolationError, OutputStream,
    PORT_RELAY_GUEST_PORT, SandboxMount, SandboxOptions, SandboxPurpose, SetupStep, VsockRoute,
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
    local_mounts: Vec<(String, String)>,
}

// The commands are the build: two layers with the same ones are the same layer,
// whichever project asked for it.
fn tag_of(steps: &[SetupStep]) -> String {
    let identity = steps
        .iter()
        .map(|step| step.command.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    blake3::hash(identity.as_bytes()).to_hex()[..12].to_owned()
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
            // opener Altitude answers, not a browser that is not there.
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
                    "Altitude created it for this project and will not put anything \
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
