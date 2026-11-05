use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env, fs,
    io::{self, ErrorKind, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
};

use directories::ProjectDirs;

use crate::{
    credentials::{Credential, Service},
    remote::InstallationKey,
};

mod serve;
mod settings;
pub use serve::{DEFAULT_PORT, LinearSettings, ServeSettings};
pub use settings::{Settings, SettingsError, SettingsFile};

#[derive(Clone, Debug)]
pub struct InstallationIdentity {
    pub id: String,
    pub label: String,
}

#[derive(Deserialize, Serialize)]
struct RecordedLabel {
    label: String,
}

// Pictures a signal carries, by content: the digest in the signal is the whole
// reference, so the same picture sent twice is one file, and a machine that holds the
// digest needs nothing else to name it. The layout is named once here for the parts
// of the system that hold a data directory but no config.
pub fn attachments_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("attachments")
}

#[derive(Clone)]
pub struct Config {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    scope: ConfigScope,
}

#[derive(Clone)]
enum ConfigScope {
    Personal,
    Remote,
    Operator(PathBuf),
}

impl Config {
    pub fn load() -> io::Result<Self> {
        let directories = ProjectDirs::from("", "", "altitude")
            .ok_or_else(|| io::Error::other("could not determine altitude's data directory"))?;
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or_else(|| io::Error::other("could not determine altitude's config directory"))?;

        let config_dir = env::var_os("ALTITUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_home.join("altitude"));
        Ok(Self {
            config_dir,
            data_dir: env::var_os("ALTITUDE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| directories.data_dir().to_owned()),
            cache_dir: env::var_os("ALTITUDE_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| directories.cache_dir().to_owned()),
            scope: ConfigScope::Personal,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn has_lost_identity(&self) -> bool {
        self.data_dir.join("identity").is_file() && !self.data_dir.join("identity.key").is_file()
    }

    pub fn forget_installation(&self) -> io::Result<PathBuf> {
        let name = self.data_dir.file_name().map_or_else(
            || "altitude".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let aside = self
            .data_dir
            .with_file_name(format!("{name}-orphaned-{stamp}"));
        fs::rename(&self.data_dir, &aside)?;
        Ok(aside)
    }

    pub fn serve(&self) -> Self {
        Self {
            config_dir: self.config_dir.clone(),
            data_dir: self.data_dir.join("serve"),
            cache_dir: self.cache_dir.join("serve"),
            scope: ConfigScope::Remote,
        }
    }

    pub fn for_operator(&self, owner: &str) -> io::Result<Self> {
        if !is_safe_operator_key(owner) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{owner:?} is not a usable operator key"),
            ));
        }
        let operator_dir = self.data_dir.join("operators").join(owner);
        Ok(Self {
            config_dir: self.config_dir.clone(),
            data_dir: self.data_dir.clone(),
            cache_dir: self.cache_dir.clone(),
            scope: ConfigScope::Operator(operator_dir),
        })
    }

    pub(crate) fn serves_operators(&self) -> bool {
        !matches!(self.scope, ConfigScope::Personal)
    }

    pub fn serve_settings_path(&self) -> PathBuf {
        self.config_dir.join("serve.toml")
    }

    pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.data_dir = data_dir;
        self
    }

    pub fn identity(&self) -> io::Result<InstallationIdentity> {
        let path = self.data_dir.join("identity");
        match fs::read_to_string(&path) {
            Ok(recorded) => {
                let RecordedLabel { label } = serde_json::from_str(&recorded).map_err(|error| {
                    io::Error::other(format!(
                        "{} is not a valid identity: {error}",
                        path.display()
                    ))
                })?;
                Ok(InstallationIdentity {
                    id: self.key()?.id().to_string(),
                    label,
                })
            }
            Err(error) if error.kind() == ErrorKind::NotFound => self.claim_identity(&path),
            Err(error) => Err(error),
        }
    }

    fn key(&self) -> io::Result<InstallationKey> {
        let path = self.data_dir.join("identity.key");
        InstallationKey::load_or_create(&path).map_err(|error| {
            io::Error::other(format!(
                "this installation's key at {} could not be read: {error}. Restore it from a \
                 backup if you have one. Minting a new key would change which installation \
                 this is, and the tasks already delegated under the old id would stay theirs.",
                path.display()
            ))
        })
    }

    fn claim_identity(&self, path: &Path) -> io::Result<InstallationIdentity> {
        let label = installation_label();
        let recorded = serde_json::to_string_pretty(&RecordedLabel {
            label: label.clone(),
        })
        .map_err(|error| io::Error::other(format!("could not encode an identity: {error}")))?;
        fs::create_dir_all(&self.data_dir)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(recorded.as_bytes())?;
                file.sync_all()?;
                tracing::info!(label = %label, "naming this installation");
                Ok(InstallationIdentity {
                    id: self.key()?.id().to_string(),
                    label,
                })
            }
            // Another Altitude instance named this installation first; keep its name.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => self.identity(),
            Err(error) => Err(error),
        }
    }

    pub fn settings_path(&self) -> PathBuf {
        match &self.scope {
            ConfigScope::Personal => self.config_dir.join("config.toml"),
            ConfigScope::Remote => self.config_dir.join("serve/config.toml"),
            ConfigScope::Operator(directory) => directory.join("config.toml"),
        }
    }

    // The settings as they are written, for a reader that must not create or
    // rewrite them: a turn looking up a login is not an edit.
    pub fn settings_if_written(&self) -> Option<Settings> {
        toml::from_str(&fs::read_to_string(self.settings_path()).ok()?).ok()
    }

    pub fn load_or_create_settings(&self) -> Result<SettingsFile, SettingsError> {
        // Profiles already on disk are the identities a pre-credential settings
        // file did not name. A fresh installation has none, and onboarding creates
        // what the operator chooses.
        let existing = Service::ALL
            .iter()
            .flat_map(|service| self.logins_on_disk(*service))
            .collect::<Vec<_>>();
        SettingsFile::load_or_create(self.settings_path(), &existing)
    }

    // Every login this installation could have on disk for one service: the
    // service's own profile, and each identity named under it. Both nest the same
    // way, so a directory that holds a profile is a login and nothing else is.
    fn logins_on_disk(&self, service: Service) -> Vec<Credential> {
        let mut found = Vec::new();
        if self.profile_dir(service.id()).is_dir() {
            found.push(Credential::new(service.id(), service.label()));
        }
        let Ok(entries) = fs::read_dir(self.scoped_data_dir().join(service.id())) else {
            return found;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let id = format!("{}/{name}", service.id());
            // A name the settings file could not read back is not a login this
            // build should offer: the directory is left where it is.
            if !self.profile_dir(&id).is_dir()
                || !Credential::new(id.clone(), name.clone()).is_valid()
            {
                continue;
            }
            found.push(Credential::new(id, name));
        }
        found.sort_by(|left, right| left.id.cmp(&right.id));
        found
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("altitude.db")
    }

    pub fn isolation_data_dir(&self) -> PathBuf {
        self.data_dir.join("isolation")
    }

    pub fn isolation_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("isolation")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.cache_dir.join("logs")
    }

    pub fn git_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("git")
    }

    pub fn microsandbox_socket_dir(&self) -> PathBuf {
        self.cache_dir.join("msb")
    }

    pub fn task_snapshots_dir(&self) -> PathBuf {
        self.data_dir.join("snapshots")
    }

    pub fn attachments_dir(&self) -> PathBuf {
        attachments_dir(&self.data_dir)
    }

    pub fn agent_channel_dir(&self) -> PathBuf {
        self.cache_dir.join("control")
    }

    pub fn profile_dir(&self, profile: &str) -> PathBuf {
        self.scoped_data_dir().join(profile).join("profile")
    }

    pub fn remove_profile(&self, id: &str) -> io::Result<()> {
        let Some(root) = self.profile_root(id) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{id:?} is not a safe profile id"),
            ));
        };
        match fs::remove_dir_all(&root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn move_profile(&self, from: &str, to: &str) -> io::Result<()> {
        let (Some(source), Some(target)) = (self.profile_root(from), self.profile_root(to)) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a profile id must be a confined relative path",
            ));
        };
        if !source.exists() {
            return Ok(());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(source, target)
    }

    fn profile_root(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty()
            || Path::new(id)
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return None;
        }
        self.profile_dir(id).parent().map(Path::to_owned)
    }

    pub fn base_snapshots_dir(&self) -> PathBuf {
        self.data_dir
            .join("environments")
            .join("workspace")
            .join("snapshots")
    }

    // What the agent and provider profiles put in a machine is the same software
    // whatever project asks for it, so it is kept once for the whole machine
    // rather than rebuilt inside every project that happens to want it.
    pub fn installs_snapshots_dir(&self) -> PathBuf {
        self.data_dir
            .join("environments")
            .join("installs")
            .join("snapshots")
    }

    pub fn project_snapshots_dir(&self, project_id: &str) -> PathBuf {
        self.scoped_data_dir()
            .join("projects")
            .join(project_id)
            .join("snapshots")
    }

    // One store per set of commands, named after them. A project whose setup command
    // changes never composes its previous tag again, so nothing that knows the store
    // exists goes looking for it: only a walk of these roots finds it.
    pub(crate) fn environment_store_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.installs_snapshots_dir()];
        let Ok(projects) = fs::read_dir(self.scoped_data_dir().join("projects")) else {
            return roots;
        };
        for project in projects.flatten() {
            roots.push(project.path().join("snapshots"));
        }
        roots
    }

    pub fn forget_profiles(&self, profiles: &[String]) -> io::Result<()> {
        for profile in profiles {
            if Path::new(profile)
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("{profile:?} is not a profile name"),
                ));
            }
            match fs::remove_dir_all(self.scoped_data_dir().join(profile)) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(crate) fn scoped_data_dir(&self) -> &Path {
        match &self.scope {
            ConfigScope::Operator(directory) => directory,
            ConfigScope::Personal | ConfigScope::Remote => &self.data_dir,
        }
    }

    pub fn prune_projects(&self, keep: &HashSet<String>) -> io::Result<Vec<String>> {
        let directory = self.scoped_data_dir().join("projects");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut removed = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if keep.contains(&id) || !is_safe_project_id(&id) {
                continue;
            }
            fs::remove_dir_all(entry.path())?;
            removed.push(id);
        }
        Ok(removed)
    }

    pub fn forget_project(&self, id: &str) -> io::Result<()> {
        if !is_safe_project_id(id) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("{id:?} is not a project id"),
            ));
        }
        match fs::remove_dir_all(self.scoped_data_dir().join("projects").join(id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn project_cache_dir(&self, project_id: &str) -> PathBuf {
        self.scoped_data_dir()
            .join("projects")
            .join(project_id)
            .join("cache")
    }
}

fn is_safe_project_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

fn is_safe_operator_key(owner: &str) -> bool {
    // Canonical transport keys prevent both path traversal and case-folding aliases.
    owner
        .parse::<iroh::EndpointId>()
        .is_ok_and(|id| id.to_string() == owner)
}

fn installation_label() -> String {
    let user = name_or_unknown(whoami::username().unwrap_or_default());
    let host = name_or_unknown(whoami::hostname().unwrap_or_default());
    format!("{user}@{host}")
}

fn name_or_unknown(name: String) -> String {
    let name: String = name.split_whitespace().collect();
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name
    }
}
