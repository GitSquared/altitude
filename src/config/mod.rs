use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, ErrorKind, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

use crate::remote::InstallationKey;

mod serve;
mod settings;
pub use serve::{DEFAULT_PORT, LinearSettings, ServeSettings};
pub use settings::{Settings, SettingsError, SettingsFile};

/// How this installation names itself.
///
/// Two things travel together because they answer different questions about the
/// same installation. The label is derived from the machine — who is at it, and
/// which machine it is — so a human reading a task's history or a tracker
/// comment knows where the work is. The id is what a machine checks, and it is
/// deliberately not derived: a name anyone can type is not an identity.
///
/// The id is the installation's public key, and the keypair that proves it lives beside
/// the label in the data directory. A name anyone can type is not an identity,
/// which is why the label is only ever for reading.
#[derive(Clone, Debug)]
pub struct InstallationIdentity {
    pub id: String,
    pub label: String,
}

/// What is written down: the label, and nothing else.
///
/// The id is deliberately not recorded. It is the key, and a key written down
/// beside a name is a second copy that can fall out of agreement with the first
/// — which then needs a check, and an error, and an operator to resolve it. There
/// is one source of truth instead, and nothing to keep in step.
#[derive(Deserialize, Serialize)]
struct RecordedLabel {
    label: String,
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

    /// Whether this installation has a name but no key, which is not a first
    /// start.
    ///
    /// The name is written before the key on a first start, so a name without one
    /// is a key that has gone. Minting a replacement silently is the one thing not
    /// to do: every remote knows this installation by the key, not the name.
    pub fn has_lost_identity(&self) -> bool {
        self.data_dir.join("identity").is_file() && !self.data_dir.join("identity.key").is_file()
    }

    /// Move everything this installation has recorded out of the way, so the next
    /// start is a first start.
    ///
    /// Renamed rather than deleted: this is what a person's task history and
    /// links live in, and a decision made at a prompt is not worth losing it to.
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

    /// The same installation's paths, as the serve role sees them.
    ///
    /// The serve role is not a client: it keeps its own key, its own record of who has
    /// connected and its own logs, and it reads its own settings file. One
    /// machine can be both roles without either seeing the other's state.
    pub fn serve(&self) -> Self {
        Self {
            config_dir: self.config_dir.clone(),
            data_dir: self.data_dir.join("serve"),
            cache_dir: self.cache_dir.join("serve"),
            scope: ConfigScope::Remote,
        }
    }

    /// A view scoped to one operator.
    ///
    /// A machine that runs work for several peers must not let one peer's
    /// logins or environment builds stand in for another's, so everything that
    /// is theirs moves under their own directory. Everything shared — the
    /// database, the runtime, the git cache, the workspace base — stays where it
    /// was.
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

    /// The directory an operator's own state lives under, or the machine's own.
    fn scoped_data_dir(&self) -> &Path {
        match &self.scope {
            ConfigScope::Operator(directory) => directory,
            ConfigScope::Personal | ConfigScope::Remote => &self.data_dir,
        }
    }

    pub(crate) fn serves_operators(&self) -> bool {
        !matches!(self.scope, ConfigScope::Personal)
    }

    /// Where the serve role reads its settings.
    pub fn serve_settings_path(&self) -> PathBuf {
        self.config_dir.join("serve.toml")
    }

    /// Use a different data directory, for a caller that was told to.
    pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.data_dir = data_dir;
        self
    }

    /// How this installation names itself.
    ///
    /// One installation is one operator on one machine, and this is what a task
    /// records as the identity it was delegated under. A machine that later runs
    /// work for several operators needs that to tell them apart, and a task
    /// cannot be given an identity retroactively without guessing at its own
    /// history.
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

    /// The keypair that is this installation's identity.
    ///
    /// Beside the label rather than inside it, because the two files have
    /// different natures: one is a secret that is never read by a human, the
    /// other is a name that appears in task history.
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

    /// Claim a name for the first time, privately.
    ///
    /// Kept outside the database so the name survives a task history reset, and
    /// written exclusively so two Altitude instances cannot both claim one. The
    /// label is resolved once and kept: a machine that is later renamed, or
    /// reached from a context that cannot resolve a user, keeps the name the
    /// tasks it already delegated were recorded under.
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

    pub fn load_or_create_settings(&self) -> Result<SettingsFile, SettingsError> {
        SettingsFile::load_or_create(self.settings_path())
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

    /// Where this machine keeps what it has already pulled, so the next task over
    /// the same repository does not pull it again.
    ///
    /// A cache by every rule that matters: disposable, and worth nothing that
    /// cannot be fetched again.
    pub fn git_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("git")
    }

    pub fn microsandbox_socket_dir(&self) -> PathBuf {
        self.cache_dir.join("msb")
    }

    pub fn task_snapshots_dir(&self) -> PathBuf {
        self.data_dir.join("snapshots")
    }

    /// Where each task's agent channel listens.
    ///
    /// Under the cache rather than the data directory because a Unix socket
    /// path is length-limited, and the data path plus a task id is already at
    /// the limit on macOS.
    pub fn agent_channel_dir(&self) -> PathBuf {
        self.cache_dir.join("control")
    }

    pub fn profile_dir(&self, profile: &str) -> PathBuf {
        self.scoped_data_dir().join(profile).join("profile")
    }

    /// Where the base image's snapshots are kept: what every project is built on
    /// top of.
    ///
    /// The directory still says `workspace` because it is storage, not
    /// vocabulary, and moving it would throw away a base build for a rename.
    pub fn base_snapshots_dir(&self) -> PathBuf {
        self.data_dir
            .join("environments")
            .join("workspace")
            .join("snapshots")
    }

    /// Where a project's built snapshots are kept, one directory per build.
    ///
    /// Keyed by the project's id rather than its name, so renaming a project
    /// leaves the build it is already running on where it is. A project's name is
    /// not a build either: a sign-in builds only the CLI it signs in to, and tasks
    /// build all of it, so the caller adds the directory that names the build.
    pub fn project_snapshots_dir(&self, project_id: &str) -> PathBuf {
        self.scoped_data_dir()
            .join("projects")
            .join(project_id)
            .join("snapshots")
    }

    /// Where a project's shared caches live on this machine.
    ///
    /// Disposable by design, like the git object cache: what is here is the
    /// machine's convenience, and losing it costs a build rather than any work.
    pub fn project_cache_dir(&self, project_id: &str) -> PathBuf {
        self.scoped_data_dir()
            .join("projects")
            .join(project_id)
            .join("cache")
    }
}

fn is_safe_operator_key(owner: &str) -> bool {
    // Canonical transport keys prevent both path traversal and case-folding aliases.
    owner
        .parse::<iroh::EndpointId>()
        .is_ok_and(|id| id.to_string() == owner)
}

/// The name a human reads for this installation: who is at which machine.
///
/// Best effort by design. An instance started by a service manager may have no
/// user
/// in its environment, and an installation must still be able to name itself.
fn installation_label() -> String {
    let user = name_or_unknown(whoami::username().unwrap_or_default());
    let host = name_or_unknown(whoami::hostname().unwrap_or_default());
    format!("{user}@{host}")
}

/// A name this machine could not resolve still has to read as something, and a
/// name is written into single-line records, so its whitespace goes.
fn name_or_unknown(name: String) -> String {
    let name: String = name.split_whitespace().collect();
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name
    }
}
