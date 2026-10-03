use std::{
    env, fs,
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

mod settings;
pub use settings::{SettingsError, SettingsFile};

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
    /// One installation is one operator on one machine, and this name is what a
    /// task records as the identity it was delegated under. A node that later
    /// runs work for several operators needs that to tell them apart, and a task
    /// cannot be given an identity retroactively without guessing at its own
    /// history. The keypair that proves this name to another node arrives with
    /// the transport, not here.
    pub fn identity_id(&self) -> io::Result<String> {
        let path = self.data_dir.join("identity");
        match fs::read_to_string(&path) {
            Ok(identity) => {
                let identity = identity.trim().to_owned();
                if identity.is_empty() {
                    return Err(io::Error::other(format!(
                        "{} names no identity",
                        path.display()
                    )));
                }
                Ok(identity)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => self.name_identity(&path),
            Err(error) => Err(error),
        }
    }

    /// Claim a name for the first time, privately.
    ///
    /// Kept outside the database so the name survives a task history reset, and
    /// written exclusively so two Altitude processes cannot both claim one.
    fn name_identity(&self, path: &Path) -> io::Result<String> {
        let identity = uuid::Uuid::new_v4().to_string();
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
                file.write_all(identity.as_bytes())?;
                file.sync_all()?;
                Ok(identity)
            }
            // Another Altitude process named this installation first; keep its name.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => self.identity_id(),
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
