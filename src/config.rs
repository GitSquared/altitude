use std::{
    collections::HashSet,
    env,
    error::Error,
    fmt, fs,
    io::{self, ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::development::DevelopmentEnvironment;

const DEFAULT_SETTINGS: &str = r#"default_environment = "rust"

[[environments]]
name = "rust"
setup_command = "apk add --no-cache rust cargo rustfmt rust-clippy build-base cmake"
"#;

#[derive(Clone)]
pub struct Config {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    pub default_environment: String,
    pub environments: Vec<DevelopmentEnvironment>,
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

    pub fn load_or_create_settings(&self) -> Result<Settings, SettingsError> {
        let path = self.settings_path();
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                self.write_settings_file(DEFAULT_SETTINGS)?;
                DEFAULT_SETTINGS.to_owned()
            }
            Err(error) => return Err(SettingsError::Io(error)),
        };
        let settings: Settings = toml::from_str(&contents).map_err(SettingsError::Parse)?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), SettingsError> {
        settings.validate()?;
        let contents = toml::to_string_pretty(settings).map_err(SettingsError::Serialize)?;
        self.write_settings_file(&contents)
    }

    fn write_settings_file(&self, contents: &str) -> Result<(), SettingsError> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.config_dir)
            .map_err(SettingsError::Io)?;
        let path = self.settings_path();
        let temporary = self
            .config_dir
            .join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
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
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(SettingsError::Io)
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

impl Settings {
    fn validate(&self) -> Result<(), SettingsError> {
        let mut names = HashSet::new();
        for environment in &self.environments {
            environment
                .validate()
                .map_err(|error| SettingsError::Invalid(error.to_string()))?;
            if !names.insert(environment_key(&environment.name)) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate environment name: {}",
                    environment.name
                )));
            }
        }
        if !names.contains(&environment_key(&self.default_environment)) {
            return Err(SettingsError::Invalid(format!(
                "default environment {:?} is not configured",
                self.default_environment
            )));
        }
        Ok(())
    }
}

/// The key two environment names are compared by.
///
/// An environment name becomes a snapshot directory name (`coding-{name}`), and
/// macOS filesystems are case-insensitive by default. Treating `rust` and `Rust`
/// as distinct would let both pass validation and then share one snapshot
/// directory, so distinct setup commands would silently reuse each other's
/// environment. Comparison is case-insensitive to keep one environment per
/// directory; the configured spelling is preserved everywhere it is stored, so
/// a task still records the name as written.
fn environment_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[derive(Debug)]
pub enum SettingsError {
    Io(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    Invalid(String),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not access altitude settings: {error}"),
            Self::Parse(error) => write!(formatter, "could not parse altitude settings: {error}"),
            Self::Serialize(error) => {
                write!(formatter, "could not encode altitude settings: {error}")
            }
            Self::Invalid(error) => write!(formatter, "invalid altitude settings: {error}"),
        }
    }
}

impl Error for SettingsError {}
