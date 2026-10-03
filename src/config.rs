use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

mod settings;
pub use settings::{SettingsError, SettingsFile};

/// How this installation names itself.
///
/// Two things travel together because they answer different questions about the
/// same installation. The label is derived from the machine — who is at it, and
/// which machine it is — so a human reading a task's history or a tracker
/// comment knows where the work is. The id is what a machine checks, and it is
/// deliberately not derived: a name anyone can type is not an identity.
///
/// The id is a placeholder until the transport supplies a keypair to fill it;
/// nothing outside this installation treats it as trustworthy yet.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InstallationIdentity {
    pub id: String,
    pub label: String,
}

#[derive(Clone)]
pub struct Config {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Config {
    pub fn load() -> io::Result<Self> {
        let directories = ProjectDirs::from("", "", "altitude")
            .ok_or_else(|| io::Error::other("could not determine altitude's data directory"))?;
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or_else(|| io::Error::other("could not determine altitude's config directory"))?;

        Ok(Self {
            config_dir: env::var_os("ALTITUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| config_home.join("altitude")),
            data_dir: env::var_os("ALTITUDE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| directories.data_dir().to_owned()),
            cache_dir: env::var_os("ALTITUDE_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| directories.cache_dir().to_owned()),
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// How this installation names itself.
    ///
    /// One installation is one operator on one machine, and this is what a task
    /// records as the identity it was delegated under. A node that later runs
    /// work for several operators needs that to tell them apart, and a task
    /// cannot be given an identity retroactively without guessing at its own
    /// history.
    pub fn identity(&self) -> io::Result<InstallationIdentity> {
        let path = self.data_dir.join("identity");
        match fs::read_to_string(&path) {
            Ok(recorded) => serde_json::from_str(&recorded).map_err(|error| {
                io::Error::other(format!(
                    "{} is not a valid identity: {error}",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => self.name_identity(&path),
            Err(error) => Err(error),
        }
    }

    /// Claim a name for the first time, privately.
    ///
    /// Kept outside the database so the name survives a task history reset, and
    /// written exclusively so two Altitude processes cannot both claim one. The
    /// label is resolved once and kept: a machine that is later renamed, or
    /// reached from a context that cannot resolve a user, keeps the name the
    /// tasks it already delegated were recorded under.
    fn name_identity(&self, path: &Path) -> io::Result<InstallationIdentity> {
        let label = install_label();
        let identity = InstallationIdentity {
            id: uuid::Uuid::new_v4().to_string(),
            label: label.clone(),
        };
        let recorded = serde_json::to_string_pretty(&identity)
            .map_err(|error| io::Error::other(format!("could not encode an identity: {error}")))?;
        fs::create_dir_all(&self.data_dir)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(recorded.as_bytes())?;
                file.sync_all()?;
                tracing::info!(label = %label, "naming this installation");
                Ok(identity)
            }
            // Another Altitude process named this installation first; keep its name.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => self.identity(),
            Err(error) => Err(error),
        }
    }

    pub fn settings_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
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

    pub fn microsandbox_socket_dir(&self) -> PathBuf {
        self.cache_dir.join("msb")
    }

    pub fn task_snapshots_dir(&self) -> PathBuf {
        self.data_dir.join("snapshots")
    }

    pub fn profile_dir(&self, profile: &str) -> PathBuf {
        self.data_dir.join(profile).join("profile")
    }

    pub fn environment_snapshots_dir(&self, name: &str) -> PathBuf {
        self.data_dir
            .join("environments")
            .join(name)
            .join("snapshots")
    }

    pub fn development_environment_snapshots_dir(&self, name: &str) -> PathBuf {
        self.data_dir
            .join("environments")
            .join("coding")
            .join(name)
            .join("snapshots")
    }
}

/// The name a human reads for this installation: who is at which machine.
///
/// Best effort by design. A node started by a service manager may have no user
/// in its environment, and an installation must still be able to name itself.
/// Whitespace is removed because the name is written into a single-line record
/// wherever it appears.
fn install_label() -> String {
    let described = |value: String| match value.split_whitespace().collect::<String>() {
        empty if empty.is_empty() => "unknown".to_owned(),
        name => name,
    };
    let user = described(whoami::username().unwrap_or_default());
    let host = described(gethostname::gethostname().to_string_lossy().into_owned());
    format!("{user}@{host}")
}
