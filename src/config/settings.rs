use crate::{
    agent::AgentKind,
    credentials::{Credential, Service, Slot, agent_of},
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
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    interface: Interface,
    #[serde(default)]
    projects: Vec<Project>,
    /// Commit identities projects have used, so a new project can reuse one
    /// rather than being typed again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    identities: Vec<ProjectCommits>,
    #[serde(default)]
    credentials: Vec<Credential>,
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
            credentials: Vec::new(),
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
    /// The coding-agent credential new tasks use. Absent is the default for the
    /// service the old `agent` field names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs_credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker_credential: Option<String>,
    /// The coding agent Altitude drives.
    ///
    /// Independent of projects: what is installed and what works in it are
    /// different questions, and either harness is useful in the same project.
    /// Absent means nobody has chosen yet, which is the one thing an interface
    /// cannot guess on someone's behalf.
    pub agent: Option<AgentKind>,
}

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
    /// Fill in what a file written before credentials existed did not say.
    ///
    /// A fresh installation starts with no identities; onboarding creates the
    /// ones the operator chooses. Profiles already on disk are adopted once, so an
    /// installation that predates credentials keeps its logins. A project that
    /// named a service by kind keeps meaning that service's adopted identity.
    fn normalize(&mut self, existing: &[Credential]) {
        if self.credentials.is_empty() {
            self.credentials = existing.to_vec();
        }
        let known = |id: &str| {
            self.credentials
                .iter()
                .any(|credential| credential.id == id)
        };
        if self
            .defaults
            .agent_credential
            .as_deref()
            .is_none_or(|id| !known(id))
        {
            self.defaults.agent_credential = self
                .defaults
                .agent
                .map(|agent| format!("agents/{}", agent_slug(agent)))
                .filter(|id| known(id));
        }
        if self
            .defaults
            .vcs_credential
            .as_deref()
            .is_none_or(|id| !known(id))
        {
            self.defaults.vcs_credential = Some("vcs/github".to_owned()).filter(|id| known(id));
        }
        if self
            .defaults
            .tracker_credential
            .as_deref()
            .is_none_or(|id| !known(id))
        {
            self.defaults.tracker_credential =
                Some("trackers/linear".to_owned()).filter(|id| known(id));
        }
        for project in &mut self.projects {
            if project
                .agent_credential
                .as_deref()
                .is_none_or(|id| !known(id))
            {
                project.agent_credential = project
                    .agent
                    .map(|agent| format!("agents/{}", agent_slug(agent)))
                    .filter(|id| known(id))
                    .or_else(|| self.defaults.agent_credential.clone());
            }
            if project
                .vcs_credential
                .as_deref()
                .is_none_or(|id| !known(id))
            {
                project.vcs_credential = self.defaults.vcs_credential.clone();
            }
            if let Some(binding) = &mut project.tracker
                && binding.credential.as_deref().is_none_or(|id| !known(id))
            {
                binding.credential = self.defaults.tracker_credential.clone();
            }
        }
        // A project answers what it runs as soon as it is read, so no screen has
        // to work out an answer the file could have said. The identity decides
        // it: a login for Pi is a project that runs Pi.
        let installation = self.defaults.agent;
        for project in &mut self.projects {
            project.agent = project
                .agent
                .or(agent_of(project.agent_credential.as_deref()));
            project.agent = project.agent.or(installation);
        }
        // One login for a slot is that slot's default: there is nothing to choose
        // between, and a default nobody set is what makes a connected tracker look
        // like a feature the build does not have.
        for slot in [Slot::Agent, Slot::VersionControl, Slot::Tracker] {
            if self.default_credential(slot).is_some() {
                continue;
            }
            let only: Vec<String> = self
                .credentials
                .iter()
                .filter(|credential| {
                    credential
                        .service()
                        .is_some_and(|service| service.slot() == slot)
                })
                .map(|credential| credential.id.clone())
                .collect();
            if let [id] = only.as_slice() {
                let _ = self.set_default_credential(id);
            }
        }
    }

    pub fn credentials(&self) -> &[Credential] {
        &self.credentials
    }

    pub fn credential(&self, id: &str) -> Option<&Credential> {
        self.credentials
            .iter()
            .find(|credential| credential.id == id)
    }

    pub fn default_credential(&self, slot: Slot) -> Option<&str> {
        match slot {
            Slot::Agent => self.defaults.agent_credential.as_deref(),
            Slot::VersionControl => self.defaults.vcs_credential.as_deref(),
            Slot::Tracker => self.defaults.tracker_credential.as_deref(),
        }
    }

    pub fn add_credential(&mut self, credential: Credential) -> Result<(), SettingsError> {
        if !credential.is_valid() {
            return Err(SettingsError::Invalid(format!(
                "credential {:?} is not a usable named identity",
                credential.id
            )));
        }
        if self.credential(&credential.id).is_some() {
            return Err(SettingsError::Invalid(format!(
                "credential {:?} already exists",
                credential.id
            )));
        }
        self.credentials.push(credential);
        Ok(())
    }

    pub fn rename_credential(&mut self, id: &str, label: &str) -> Result<(), SettingsError> {
        let label = label.trim();
        if label.is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
            return Err(SettingsError::Invalid(
                "a name needs one short line".to_owned(),
            ));
        }
        if self
            .credentials
            .iter()
            .any(|credential| credential.id != id && credential.label.eq_ignore_ascii_case(label))
        {
            return Err(SettingsError::Invalid(format!("{label:?} is already used")));
        }
        let Some(credential) = self
            .credentials
            .iter_mut()
            .find(|credential| credential.id == id)
        else {
            return Err(SettingsError::Invalid(format!("no credential {id:?}")));
        };
        credential.label = label.to_owned();
        credential.label_auto = false;
        Ok(())
    }

    /// Give an identity the id its account name implies, and carry every
    /// reference to it along.
    pub fn reidentify(&mut self, old: &str, new: &str, label: &str) -> Result<(), SettingsError> {
        if old == new {
            self.suggest_credential_name(old, label);
            return Ok(());
        }
        let candidate = Credential::new(new, label);
        if !candidate.is_valid() {
            return Err(SettingsError::Invalid(format!(
                "{new:?} is not a usable identity id"
            )));
        }
        if self.credential(new).is_some() {
            return Err(SettingsError::Invalid(format!(
                "identity {new:?} already exists"
            )));
        }
        let Some(credential) = self.credentials.iter_mut().find(|c| c.id == old) else {
            return Err(SettingsError::Invalid(format!("no credential {old:?}")));
        };
        credential.id = new.to_owned();
        credential.label = label.to_owned();
        for default in [
            &mut self.defaults.agent_credential,
            &mut self.defaults.vcs_credential,
            &mut self.defaults.tracker_credential,
        ] {
            if default.as_deref() == Some(old) {
                *default = Some(new.to_owned());
            }
        }
        for project in &mut self.projects {
            if project.agent_credential.as_deref() == Some(old) {
                project.agent_credential = Some(new.to_owned());
            }
            if project.vcs_credential.as_deref() == Some(old) {
                project.vcs_credential = Some(new.to_owned());
            }
            if let Some(binding) = &mut project.tracker
                && binding.credential.as_deref() == Some(old)
            {
                binding.credential = Some(new.to_owned());
            }
        }
        Ok(())
    }

    pub fn suggest_credential_name(&mut self, id: &str, label: &str) {
        let label = label.trim();
        if label.is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
            return;
        }
        if self
            .credentials
            .iter()
            .any(|credential| credential.id != id && credential.label.eq_ignore_ascii_case(label))
        {
            return;
        }
        if let Some(credential) = self
            .credentials
            .iter_mut()
            .find(|credential| credential.id == id)
            && credential.label_auto
        {
            credential.label = label.to_owned();
        }
    }

    pub fn set_default_credential(&mut self, id: &str) -> Result<(), SettingsError> {
        let Some(credential) = self.credential(id) else {
            return Err(SettingsError::Invalid(format!("no credential {id:?}")));
        };
        match credential.service().map(|service| service.slot()) {
            Some(Slot::Agent) => self.defaults.agent_credential = Some(id.to_owned()),
            Some(Slot::VersionControl) => self.defaults.vcs_credential = Some(id.to_owned()),
            Some(Slot::Tracker) => self.defaults.tracker_credential = Some(id.to_owned()),
            None => {
                return Err(SettingsError::Invalid(format!(
                    "{id:?} is not a known service"
                )));
            }
        }
        Ok(())
    }

    /// The projects that run with a credential.
    ///
    /// Named rather than counted: an identity cannot be forgotten while a project
    /// still reads it, and "a project" sends the operator looking for which one.
    pub fn projects_binding(&self, id: &str) -> Vec<&str> {
        self.projects
            .iter()
            .filter(|project| {
                project.agent_credential.as_deref() == Some(id)
                    || project.vcs_credential.as_deref() == Some(id)
                    || project
                        .tracker
                        .as_ref()
                        .and_then(|binding| binding.credential.as_deref())
                        == Some(id)
            })
            .map(|project| project.name.as_str())
            .collect()
    }

    /// Why an identity cannot be forgotten yet, naming what still reads it.
    pub fn binding_refusal(&self, id: &str) -> Option<String> {
        let bound = self.projects_binding(id);
        if bound.is_empty() {
            return None;
        }
        let one = bound.len() == 1;
        Some(format!(
            "{} still {} this identity; change {} first",
            named(&bound),
            if one { "binds" } else { "bind" },
            if one {
                "the project's login"
            } else {
                "their logins"
            },
        ))
    }

    pub fn remove_credential(&mut self, id: &str) -> Result<(), SettingsError> {
        if let Some(refusal) = self.binding_refusal(id) {
            return Err(SettingsError::Invalid(refusal));
        }
        // A tracker is optional, so its default may go with its last identity.
        // An agent and a version-control login are what a task runs with, so
        // those keep the default until another identity takes it over.
        if self.defaults.tracker_credential.as_deref() == Some(id) {
            self.defaults.tracker_credential = None;
        } else if [
            self.defaults.agent_credential.as_deref(),
            self.defaults.vcs_credential.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|default| default == id)
        {
            return Err(SettingsError::Invalid(
                "this identity is a default; make another one the default first".to_owned(),
            ));
        }
        self.credentials.retain(|credential| credential.id != id);
        Ok(())
    }

    /// The credential ids this file's projects run with, which is what a remote
    /// is sent.
    ///
    /// The installation's defaults are deliberately not part of this: they are
    /// templates a new project is written from, not identities a task reads. A
    /// machine is sent the logins of the work it runs, so a default that no
    /// project here names would be a key in someone else's hand for no task.
    ///
    /// A project that names no identity falls back to what its harness or
    /// provider implies, because that is what the task will resolve to.
    pub fn used_credential_ids(&self) -> Vec<String> {
        let mut used = Vec::new();
        for project in &self.projects {
            let agent = project
                .agent_credential
                .clone()
                .or_else(|| project.agent.map(|kind| kind.profile().to_owned()));
            let vcs = project
                .vcs_credential
                .clone()
                .or_else(|| project.vcs.map(|kind| kind.profile().to_owned()));
            let tracker = project
                .tracker
                .as_ref()
                .and_then(|binding| binding.credential.clone());
            used.extend(agent.into_iter().chain(vcs).chain(tracker));
        }
        used.sort_unstable();
        used.dedup();
        used
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    pub fn remotes(&self) -> &[String] {
        &self.remotes
    }

    /// Keep only the projects a machine is the one that runs.
    ///
    /// A remote is sent what it is for: its own projects' setup, repository, and
    /// tracker binding. Everything else stays on the machine the work belongs to,
    /// which is what keeps a day-job project's settings off a personal machine.
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

    pub fn agent(&self) -> Option<AgentKind> {
        self.defaults.agent
    }

    pub fn set_agent(&mut self, agent: AgentKind) {
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

    /// Remove a saved commit identity, leaving projects that used it on the
    /// installation's identity.
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

    /// Replace a saved commit identity and every project bound to it.
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

    pub fn set_keyboard_hints(&mut self, on: bool) {
        self.interface.keyboard_hints = on;
    }

    pub fn set_animations(&mut self, on: bool) {
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

    /// Remove a project.
    ///
    /// The last one stays: every task belongs to a project, and settings with
    /// none cannot found one. Validation refuses the empty list, so an operator
    /// who deletes the last project is told rather than left with a file the next
    /// start would have to invent a project for.
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

    fn validate(&self) -> Result<(), SettingsError> {
        let mut credential_ids = HashSet::new();
        for credential in &self.credentials {
            if !credential.is_valid() {
                return Err(SettingsError::Invalid(format!(
                    "credential {:?} is not a usable named identity",
                    credential.id
                )));
            }
            if !credential_ids.insert(credential.id.as_str()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate credential id: {}",
                    credential.id
                )));
            }
        }
        for (slot, id) in [
            (Slot::Agent, self.defaults.agent_credential.as_deref()),
            (
                Slot::VersionControl,
                self.defaults.vcs_credential.as_deref(),
            ),
            (Slot::Tracker, self.defaults.tracker_credential.as_deref()),
        ] {
            if let Some(id) = id
                && (!credential_ids.contains(id)
                    || Service::from_id(id).is_none_or(|service| service.slot() != slot))
            {
                return Err(SettingsError::Invalid(format!(
                    "default credential {id:?} does not name a {slot:?} identity"
                )));
            }
        }
        for project in &self.projects {
            for (slot, id) in [
                (Slot::Agent, project.agent_credential.as_deref()),
                (Slot::VersionControl, project.vcs_credential.as_deref()),
                (
                    Slot::Tracker,
                    project
                        .tracker
                        .as_ref()
                        .and_then(|binding| binding.credential.as_deref()),
                ),
            ] {
                if let Some(id) = id
                    && (!credential_ids.contains(id)
                        || Service::from_id(id).is_none_or(|service| service.slot() != slot))
                {
                    return Err(SettingsError::Invalid(format!(
                        "project {:?} binds {id:?}, which is not a {slot:?} identity",
                        project.name
                    )));
                }
            }
        }
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
        // Two projects claiming one scope is a question nobody can answer when the
        // issue arrives, so it is refused while the file is being written. A binding
        // with no scope claims its whole account, and only one project may.
        let mut claims: Vec<(String, &str)> = Vec::new();
        for project in &self.projects {
            let Some(binding) = &project.tracker else {
                continue;
            };
            let key = binding.claim_key();
            let what = binding.scope.as_deref().unwrap_or("the whole account");
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
fn agent_slug(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "claude",
        AgentKind::Pi => "pi",
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

/// A list of names as a sentence reads it: "altitude", "altitude and api",
/// "altitude, api and web".
fn named(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}
