use crate::development::DevelopmentEnvironment;
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

const DEFAULT_SETTINGS: &str = r#"default_environment = "rust"

[[environments]]
name = "rust"
setup_command = "apk add --no-cache rust cargo rustfmt rust-clippy build-base cmake"
"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    pub default_environment: String,
    pub environments: Vec<DevelopmentEnvironment>,
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
                write_settings(&path, DEFAULT_SETTINGS)?;
                DEFAULT_SETTINGS.to_owned()
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
