use crate::{
    agent::AgentKind,
    credentials::{Credential, Service, Slot, agent_of},
    project::{Project, ProjectCommits},
    tracker::TrackerKind,
    vcs::VcsProviderKind,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remotes: Vec<String>,
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    interface: Interface,
    #[serde(default)]
    projects: Vec<Project>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    identities: Vec<ProjectCommits>,
    #[serde(default)]
    credentials: Vec<Credential>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            remotes: Vec::new(),
            defaults: Defaults::default(),
            interface: Interface::default(),
            projects: vec![Project::new(String::new())],
            identities: Vec::new(),
            credentials: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs_credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker_credential: Option<String>,
    pub agent: Option<AgentKind>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Interface {
    pub keyboard_hints: bool,
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

pub struct SettingsFile {
    path: PathBuf,
    source: String,
    settings: Settings,
}

impl SettingsFile {
    pub(super) fn load_or_create(
        path: PathBuf,
        existing: &[Credential],
    ) -> Result<Self, SettingsError> {
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
        let mut settings: Settings = toml::from_str(&source).map_err(SettingsError::Parse)?;
        settings.normalize(existing);
        settings.validate()?;
        let normalized = toml::to_string_pretty(&settings).map_err(SettingsError::Serialize)?;
        let source = if normalized == source {
            source
        } else {
            write_settings(&path, &normalized)?;
            normalized
        };
        Ok(Self {
            path,
            source,
            settings,
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn save(&mut self, settings: Settings) -> Result<(), SettingsError> {
        settings.validate()?;
        // The stable lock file serializes altitude writers across atomic renames.
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

mod credentials;
mod validate;

impl Settings {
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    pub fn remotes(&self) -> &[String] {
        &self.remotes
    }

    pub fn retain_projects_for(&mut self, host: &str) {
        self.projects
            .retain(|project| project.host.as_deref() == Some(host));
    }

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

    pub fn agent(&self) -> Option<AgentKind> {
        self.defaults.agent
    }

    pub fn choose_agent(&mut self, agent: AgentKind) {
        self.defaults.agent = Some(agent);
    }

    pub fn identities(&self) -> &[ProjectCommits] {
        &self.identities
    }

    pub fn remember_identity(&mut self, commits: &ProjectCommits) {
        if let Some(known) = self
            .identities
            .iter_mut()
            .find(|known| known.email.eq_ignore_ascii_case(&commits.email))
        {
            *known = commits.clone();
        } else {
            self.identities.push(commits.clone());
        }
    }

    pub fn remove_identity(&mut self, commits: &ProjectCommits) {
        let matches = |known: &ProjectCommits| {
            known == commits || known.email.eq_ignore_ascii_case(&commits.email)
        };
        self.identities.retain(|known| !matches(known));
        for project in &mut self.projects {
            if project.commits.as_ref().is_some_and(matches) {
                project.commits = None;
            }
        }
    }

    pub fn replace_identity(&mut self, old: &ProjectCommits, new: ProjectCommits) {
        let matches =
            |known: &ProjectCommits| known == old || known.email.eq_ignore_ascii_case(&old.email);
        self.identities.retain(|known| !matches(known));
        self.remember_identity(&new);
        for project in &mut self.projects {
            if project.commits.as_ref().is_some_and(matches) {
                project.commits = Some(new.clone());
            }
        }
    }

    pub fn interface(&self) -> &Interface {
        &self.interface
    }

    pub fn show_keyboard_hints(&mut self, on: bool) {
        self.interface.keyboard_hints = on;
    }

    pub fn animate_interface(&mut self, on: bool) {
        self.interface.animations = on;
    }

    pub fn add_project(&mut self, mut project: Project) {
        // The same answer the file would settle on load, so a project is never
        // stored in a state the next start has to correct.
        project.agent = project
            .agent
            .or(agent_of(project.agent_credential.as_deref()));
        project.agent = project.agent.or(self.defaults.agent);
        self.projects.retain(|existing| existing.id != project.id);
        self.projects.push(project);
    }

    pub fn remove_project(&mut self, id: &str) {
        self.projects.retain(|existing| existing.id != id);
    }

    pub fn add_remote(&mut self, address: &str) {
        let address = address.trim();
        self.remotes
            .retain(|existing| !existing.eq_ignore_ascii_case(address));
        self.remotes.push(address.to_owned());
    }

    pub fn remove_remote(&mut self, address: &str) {
        self.remotes
            .retain(|existing| !existing.eq_ignore_ascii_case(address));
    }
}

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

fn named(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}
