use std::{
    env, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

mod settings;
pub use settings::{Settings, SettingsError, SettingsFile};

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
