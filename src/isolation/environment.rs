use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use crate::config::Config;
use crate::environment::Environment;

use super::{
    BaseSandboxRecipe, CommandEvent, EnvironmentProgressSink, IsolationEngine, IsolationError,
    SandboxMount, SandboxOptions, SetupStep,
};

// The current Alpine environment runs as root. Resolve home-relative requirements here.
const GUEST_HOME: &str = "/root";
pub const REFRESH_AFTER: Duration = Duration::from_secs(72 * 60 * 60);

/// Knowledge supplied by an agent or VCS module. No host I/O or lifecycle hooks.
pub struct EnvironmentRecipe {
    /// Stable relative profile identity, e.g. `agents/claude` or `vcs/github`.
    pub profile: &'static str,
    /// What this recipe installs, in the operator's words; a waiting task shows it.
    pub label: &'static str,
    pub setup_command: &'static str,
    pub directories: Vec<PersistentDirectory>,
    pub environment: Vec<(&'static str, &'static str)>,
}

pub enum GuestPath {
    Absolute(&'static str),
    HomeRelative(&'static str),
}

pub struct PersistentDirectory {
    /// Relative path inside the profile.
    pub relative_path: &'static str,
    pub guest_path: GuestPath,
    pub defaults: Vec<DefaultFile>,
    pub instructions: &'static str,
    /// Redirect these guest subdirectories to the task disk, outside the shared profile.
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
            instructions,
            task_local_subdirectories: Vec::new(),
        }
    }

    pub fn with_task_local_subdirectories(mut self, paths: &[&'static str]) -> Self {
        self.task_local_subdirectories = paths.to_vec();
        self
    }
}

pub struct DefaultFile {
    pub path: &'static str,
    pub contents: &'static str,
}

struct EnvironmentPlan {
    recipe: BaseSandboxRecipe,
    options: SandboxOptions,
    defaults: Vec<(PathBuf, &'static str)>,
    readmes: Vec<(PathBuf, String)>,
    task_local_mounts: Vec<(String, String)>,
}

impl EnvironmentPlan {
    fn compose(
        config: &Config,
        environment: Environment,
        recipes: Vec<EnvironmentRecipe>,
    ) -> Result<Self, IsolationError> {
        environment
            .validate()
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let environment_name = environment.name;
        let mut plan = Self {
            recipe: BaseSandboxRecipe::new(
                format!("coding-{environment_name}"),
                config.environment_snapshots_dir(&environment_name),
                REFRESH_AFTER,
                // Each contributor to the build is its own step, so a task
                // waiting on a cold environment can say what it is waiting for.
                vec![SetupStep::new(
                    format!("preparing the {environment_name} environment"),
                    environment.setup_command,
                )],
            ),
            options: SandboxOptions {
                snapshot_path: None,
                network_enabled: true,
                mounts: Vec::new(),
                environment: vec![
                    ("HOME".into(), GUEST_HOME.into()),
                    ("SHELL".into(), "/bin/bash".into()),
                ],
            },
            defaults: Vec::new(),
            readmes: Vec::new(),
            task_local_mounts: Vec::new(),
        };
        let mut profiles = HashSet::new();
        for recipe in recipes {
            relative_path(recipe.profile, false)?;
            if !profiles.insert(recipe.profile) {
                return Err(IsolationError::new("duplicate profile recipe"));
            }
            plan.recipe
                .steps
                .push(SetupStep::new(recipe.label, recipe.setup_command));
            for directory in recipe.directories {
                relative_path(directory.relative_path, true)?;
                let host_path = config
                    .profile_dir(recipe.profile)
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
                plan.readmes.push((host_path.join("README.md"), readme));
                for path in directory.task_local_subdirectories {
                    relative_path(path, false)?;
                    let source = Path::new("/altitude/task-state")
                        .join(recipe.profile)
                        .join(directory.relative_path)
                        .join(path);
                    let target = guest_path.join(path);
                    plan.task_local_mounts.push((
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
        Ok(plan)
    }

    fn initialize(&mut self, config: &Config) -> Result<(), IsolationError> {
        private_directory(config.data_dir())?;
        for mount in &mut self.options.mounts {
            private_directory(&mount.host_path)?;
            mount.host_path = fs::canonicalize(&mount.host_path).map_err(io_error)?;
        }
        for (path, readme) in &self.readmes {
            // Replace the managed note without following a guest-created symlink.
            let temporary = path.with_file_name(format!(".README-{}", uuid::Uuid::new_v4()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(io_error)?;
            file.write_all(readme.as_bytes()).map_err(io_error)?;
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

pub(super) fn private_directory(path: &Path) -> Result<(), IsolationError> {
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

pub(super) async fn provision(
    engine: &impl IsolationEngine,
    config: &Config,
    id: String,
    snapshot: Option<String>,
    environment: Environment,
    recipes: Vec<EnvironmentRecipe>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<(), IsolationError> {
    // Validate every recipe before creating directories or building images.
    let mut plan = EnvironmentPlan::compose(config, environment, recipes)?;
    plan.initialize(config)?;
    plan.options.snapshot_path = Some(match snapshot {
        Some(path) => path,
        None => engine
            .ensure_base(plan.recipe, progress)
            .await?
            .path
            .to_string_lossy()
            .into_owned(),
    });
    engine.create(id.clone(), plan.options).await?;
    if !plan.task_local_mounts.is_empty() {
        let mut command = String::from("set -eu\n");
        for (source, target) in plan.task_local_mounts {
            let source = shell_quote(&source);
            let target = shell_quote(&target);
            command.push_str(&format!(
                "mkdir -p {source} {target}\nmount --bind {source} {target}\n"
            ));
        }
        let result = run_setup(engine, id.clone(), command).await;
        if let Err(error) = result {
            if let Err(cleanup_error) = engine.remove(id).await {
                tracing::warn!(%cleanup_error, "could not remove sandbox after task directory setup failure");
            }
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn run_setup(
    engine: &impl IsolationEngine,
    id: String,
    command: String,
) -> Result<(), IsolationError> {
    let mut session = engine.run_command(id, command).await?;
    let mut output = String::new();
    while let Some(event) = session.events.recv().await {
        match event {
            CommandEvent::Output { bytes, .. } => output.push_str(&String::from_utf8_lossy(&bytes)),
            CommandEvent::Exited(0) => return Ok(()),
            CommandEvent::Exited(code) => {
                return Err(IsolationError::new(format!(
                    "task directory setup exited with code {code}: {output}"
                )));
            }
            CommandEvent::Failed(error) => return Err(IsolationError::new(error)),
        }
    }
    Err(IsolationError::new(
        "task directory setup closed without an exit status",
    ))
}
