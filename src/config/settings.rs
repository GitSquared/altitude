use crate::{
    agent::AgentKind,
    project::{Project, ProjectCommits},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    error::Error,
    fmt, fs,
    io::{self, ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use toml::{de::Error as ParseError, ser::Error as SerializeError};

/// What is configured, as `config.toml` holds it.
///
/// Fields are private and edited through the methods below, so that the rules
/// that go with each list — there is always a project, a name appears once, an
/// address appears once — live in one place instead of at every call site.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    /// The addresses of the machines this host delegates to.
    ///
    /// An address, not a name: what a remote is *called* is what it told you,
    /// which is state rather than configuration. An address may be a hostname,
    /// and one the tailnet gave is better than an IP that will move.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remotes: Vec<String>,
    /// What a task is given when nobody says otherwise.
    #[serde(default)]
    defaults: Defaults,
    /// How the interface behaves, as opposed to what it configures.
    #[serde(default)]
    interface: Interface,
    /// The projects this installation can work on.
    #[serde(default)]
    projects: Vec<Project>,
    /// Commit identities projects have used, so a new project can reuse one
    /// rather than being typed again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    identities: Vec<ProjectCommits>,
}

impl Default for Settings {
    /// A first start has one project, because every task belongs to one and an
    /// interface with nothing to work in is not a first step.
    fn default() -> Self {
        Self {
            remotes: Vec::new(),
            defaults: Defaults::default(),
            interface: Interface::default(),
            projects: vec![Project::new(String::new())],
            identities: Vec::new(),
        }
    }
}

/// What a new task gets when nobody says otherwise.
///
/// What the work is — its setup, repository, caches, instructions, and the machine
/// its tasks start on — belongs to the project. This is what is left that is not
/// about a project.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Defaults {
    /// The coding agent Altitude drives.
    ///
    /// Independent of projects: what is installed and what works in it are
    /// different questions, and either harness is useful in the same project.
    /// Absent means nobody has chosen yet, which is the one thing an interface
    /// cannot guess on someone's behalf.
    pub agent: Option<AgentKind>,
}

/// Preferences about the interface itself.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Interface {
    /// Whether the keys that work are listed at the bottom of the screen.
    ///
    /// The `?` popup lists them either way: a key you cannot find is a key you
    /// do not have, so help is never something to be switched on first.
    pub keyboard_hints: bool,
    /// Whether the interface animates: the transitions between what is in
    /// front, and the drifting backdrop behind it.
    ///
    /// Off, the loops that draw those are not run at all, rather than drawn
    /// still, so a machine that is asked to do less actually does less.
    pub animations: bool,
}

impl Default for Interface {
    fn default() -> Self {
        Self {
            keyboard_hints: true,
            animations: true,
        }
    }
}

/// A loaded file and its exact source, used to detect edits before saving.
pub struct SettingsFile {
    path: PathBuf,
    source: String,
    settings: Settings,
}

impl SettingsFile {
    pub(super) fn load_or_create(path: PathBuf) -> Result<Self, SettingsError> {
        let _lock = lock_settings(&path)?;
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // A file the operator can read: the defaults, written out, rather
                // than a shape they have to infer from the program's behaviour.
                let source = toml::to_string_pretty(&Settings::default())
                    .map_err(SettingsError::Serialize)?;
                write_settings(&path, &source)?;
                source
            }
            Err(error) => return Err(SettingsError::Io(error)),
        };
        let settings: Settings = toml::from_str(&source).map_err(SettingsError::Parse)?;
        settings.validate()?;
        Ok(Self {
            path,
            source,
            settings,
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Adopt a proposed edit only after it has been successfully saved.
    pub fn save(&mut self, settings: Settings) -> Result<(), SettingsError> {
        settings.validate()?;
        // The stable lock file serializes Altitude writers across atomic renames.
        let _lock = lock_settings(&self.path)?;
        let current = fs::read_to_string(&self.path).map_err(SettingsError::Io)?;
        if current != self.source {
            return Err(SettingsError::Changed);
        }
        let source = toml::to_string_pretty(&settings).map_err(SettingsError::Serialize)?;
        write_settings(&self.path, &source)?;
        self.source = source;
        self.settings = settings;
        Ok(())
    }
}

fn lock_settings(path: &Path) -> Result<fs::File, SettingsError> {
    let directory = path.parent().expect("settings path has a parent directory");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(SettingsError::Io)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join(".config.lock"))
        .map_err(SettingsError::Io)?;
    lock.try_lock()
        .map_err(|error| SettingsError::Io(error.into()))?;
    Ok(lock)
}

fn write_settings(path: &Path, contents: &str) -> Result<(), SettingsError> {
    let temporary = path.with_file_name(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.map_err(SettingsError::Io)
}

impl Settings {
    /// The projects this installation works on, oldest first.
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    /// The addresses this host delegates to.
    pub fn remotes(&self) -> &[String] {
        &self.remotes
    }

    /// Keep only the projects a machine is the one that runs.
    ///
    /// A remote is sent what it is for: its own projects' setup, repository, and
    /// route. Everything else stays on the machine the work belongs to, which is
    /// what keeps a day-job project's settings off a personal machine.
    pub fn retain_projects_for(&mut self, host: &str) {
        self.projects
            .retain(|project| project.host.as_deref() == Some(host));
    }

    /// The configuration a remote is sent, which is the part it can act on.
    ///
    /// A remote builds this machine's environments, so it needs those and the
    /// agent. It has no screen, and what this one shows — the keys, the
    /// animations, the machine the last task went to — is not configuration it
    /// could use. Sending it anyway would make a preference look like a change
    /// worth syncing, and every task created would push a new configuration to
    /// every remote.
    pub fn shared(&self) -> Self {
        let mut shared = self.clone();
        shared.remotes.clear();
        shared.interface = Interface::default();
        // Where a project runs is this client's choice; the machine it is sent to
        // can only run it.
        for project in &mut shared.projects {
            project.host = None;
        }
        shared
    }

    /// The coding agent Altitude drives, if one has been chosen.
    pub fn agent(&self) -> Option<AgentKind> {
        self.defaults.agent
    }

    /// Choose the coding agent Altitude drives.
    pub fn set_agent(&mut self, agent: AgentKind) {
        self.defaults.agent = Some(agent);
    }

    /// Commit identities projects have used, so the next project can pick one.
    pub fn identities(&self) -> &[ProjectCommits] {
        &self.identities
    }

    /// Keep an identity a project uses, without duplicating one already here.
    pub fn remember_identity(&mut self, commits: &ProjectCommits) {
        if !self.identities.contains(commits) {
            self.identities.push(commits.clone());
        }
    }

    /// How the interface behaves.
    pub fn interface(&self) -> &Interface {
        &self.interface
    }

    /// Whether the keys that work are listed at the bottom of the screen.
    pub fn set_keyboard_hints(&mut self, on: bool) {
        self.interface.keyboard_hints = on;
    }

    /// Whether the interface animates.
    pub fn set_animations(&mut self, on: bool) {
        self.interface.animations = on;
    }

    /// Add a project, replacing one with the same id.
    pub fn add_project(&mut self, project: Project) {
        self.projects.retain(|existing| existing.id != project.id);
        self.projects.push(project);
    }

    /// Remove a project.
    ///
    /// The last one stays: every task belongs to a project, and settings with
    /// none cannot found one. Validation refuses the empty list, so an operator
    /// who deletes the last project is told rather than left with a file the next
    /// start would have to invent a project for.
    pub fn remove_project(&mut self, id: &str) {
        self.projects.retain(|existing| existing.id != id);
    }

    /// Add a remote address, replacing any entry for the same address.
    pub fn add_remote(&mut self, address: &str) {
        let address = address.trim();
        self.remotes
            .retain(|existing| !existing.eq_ignore_ascii_case(address));
        self.remotes.push(address.to_owned());
    }

    /// Forget a remote address.
    pub fn remove_remote(&mut self, address: &str) {
        self.remotes
            .retain(|existing| !existing.eq_ignore_ascii_case(address));
    }

    fn validate(&self) -> Result<(), SettingsError> {
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for project in &self.projects {
            project
                .validate()
                .map_err(|error| SettingsError::Invalid(error.to_string()))?;
            // A project id is a uuid, because storage is keyed by it and because
            // Altitude builds a few sandboxes under ids of its own.
            if project.id.parse::<uuid::Uuid>().is_err() {
                return Err(SettingsError::Invalid(format!(
                    "project id {:?} must be a uuid",
                    project.id
                )));
            }
            if !ids.insert(project.id.clone()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate project id: {}",
                    project.id
                )));
            }
            if !names.insert(project.key()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate project name: {}",
                    project.name
                )));
            }
        }
        // Two projects claiming one scope is a question nobody can answer at
        // routing time, so it is refused while the file is being written. A
        // route with no scope claims its whole account, and only one project may.
        let mut claims: Vec<(String, &str)> = Vec::new();
        for project in &self.projects {
            let Some(route) = &project.tracker else {
                continue;
            };
            let key = route.claim_key();
            let what = route.scope.as_deref().unwrap_or("the whole account");
            if let Some((_, other)) = claims.iter().find(|(claimed, _)| *claimed == key) {
                return Err(SettingsError::Invalid(format!(
                    "{other} and {} both claim {what}",
                    project.name
                )));
            }
            claims.push((key, &project.name));
        }
        let mut addresses = HashSet::new();
        for address in &self.remotes {
            validate_remote_address(address)?;
            if !addresses.insert(address.to_ascii_lowercase()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate remote address: {address}"
                )));
            }
        }
        Ok(())
    }
}

/// A remote is an address: where it is, and optionally the port it serves on.
///
/// Checked only far enough to catch a typo. Proving an address reaches something
/// means dialling it, which the link does and reports on its own.
fn validate_remote_address(address: &str) -> Result<(), SettingsError> {
    let invalid = |why: &str| {
        Err(SettingsError::Invalid(format!(
            "remote address {address:?} {why}"
        )))
    };
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return invalid("is empty");
    }
    if trimmed.len() > 255 {
        return invalid("is longer than 255 characters");
    }
    if trimmed.chars().any(char::is_whitespace) {
        return invalid("contains whitespace");
    }
    if trimmed.contains('/') {
        return invalid(
            "contains a path; give a host, and a port if it does not serve on the default",
        );
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((host, port)) = rest.split_once("]:") else {
            return invalid("names an address in brackets but no port after them");
        };
        if host.is_empty() || port.parse::<u16>().is_err() {
            return invalid("does not end in a port number");
        }
        return Ok(());
    }
    if trimmed.matches(':').count() > 1 {
        return invalid("looks like an IPv6 address; write it in brackets, as [::1]:25848");
    }
    if let Some((host, port)) = trimmed.rsplit_once(':') {
        if host.is_empty() {
            return invalid("has no host");
        }
        if port.parse::<u16>().is_err() {
            return invalid("does not end in a port number");
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum SettingsError {
    Io(io::Error),
    Parse(ParseError),
    Serialize(SerializeError),
    Invalid(String),
    Changed,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not access altitude settings: {error}"),
            Self::Parse(error) => write!(formatter, "could not parse altitude settings: {error}"),
            Self::Serialize(error) => {
                write!(formatter, "could not encode altitude settings: {error}")
            }
            Self::Changed => write!(
                formatter,
                "settings changed on disk; reopen settings to reload before editing"
            ),
            Self::Invalid(error) => write!(formatter, "invalid altitude settings: {error}"),
        }
    }
}

impl Error for SettingsError {}
