use crate::development::{BASE_NAME, DevelopmentEnvironment, is_base_name};
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

const DEFAULT_SETTINGS: &str = r#"default_environment = "base"
"#;

/// What is configured, as `config.toml` holds it.
///
/// The built-in base environment is deliberately absent: it cannot be edited or
/// deleted, so writing it down would record a preference nobody has. Everything
/// that lists environments uses [`Settings::environments`], which adds it.
///
/// Fields are private and edited through the methods below, so that the rules
/// that go with each list — the base cannot be configured, a default never
/// points at nothing, an address appears once — live in one place instead of at
/// every call site.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Settings {
    pub default_environment: String,
    #[serde(rename = "environments", default)]
    configured: Vec<DevelopmentEnvironment>,
    /// The addresses of the machines this host delegates to.
    ///
    /// An address, not a name: what a remote is *called* is what it told you,
    /// which is state rather than configuration.
    #[serde(default)]
    remotes: Vec<String>,
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
    /// Everything a task can run in: the built-in base, then what is configured.
    pub fn environments(&self) -> Vec<DevelopmentEnvironment> {
        let mut all = vec![DevelopmentEnvironment::base()];
        all.extend(self.configured.iter().cloned());
        all
    }

    /// The addresses this host delegates to.
    pub fn remotes(&self) -> &[String] {
        &self.remotes
    }

    /// Add a configured environment, replacing one of the same name.
    pub fn add_environment(&mut self, environment: DevelopmentEnvironment) {
        self.configured
            .retain(|existing| !existing.same_name(&environment));
        self.configured.push(environment);
    }

    /// Remove a configured environment.
    // (the attribute sits on the item below)
    ///
    ///
    /// The default is never left pointing at nothing: removing the default hands
    /// it back to the base, which always exists.
    pub fn remove_environment(&mut self, name: &str) {
        self.configured
            .retain(|existing| !existing.name.eq_ignore_ascii_case(name));
        if self.default_environment.eq_ignore_ascii_case(name) {
            self.default_environment = BASE_NAME.to_owned();
        }
    }

    /// Choose the environment a new task runs in unless one is picked.
    pub fn set_default_environment(&mut self, name: &str) {
        self.default_environment = name.to_owned();
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
        let mut names = HashSet::new();
        for environment in &self.configured {
            environment
                .validate()
                .map_err(|error| SettingsError::Invalid(error.to_string()))?;
            if environment.is_base() {
                return Err(SettingsError::Invalid(format!(
                    "{BASE_NAME} is built in and cannot be configured"
                )));
            }
            if !names.insert(environment.key()) {
                return Err(SettingsError::Invalid(format!(
                    "duplicate environment name: {}",
                    environment.name
                )));
            }
        }
        // The base is always available, so a default that resolves to nothing is
        // a mistake to report rather than a condition to fall back from.
        if !is_base_name(&self.default_environment)
            && !names.contains(&self.default_environment.to_ascii_lowercase())
        {
            return Err(SettingsError::Invalid(format!(
                "default environment {:?} is not configured",
                self.default_environment
            )));
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
