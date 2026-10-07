use crate::agent::AgentKind;
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

/// The built-in environment's name.
///
/// Reserved: it cannot be configured, because the built-in environment is always
/// present. See [`DevelopmentEnvironment::base`].
pub const BASE_NAME: &str = "base";

/// A named environment a task runs in, as configured in `config.toml`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevelopmentEnvironment {
    pub name: String,
    pub setup_command: String,
    /// The coding harness Altitude drives in this environment. Isolation
    /// ignores it; task execution uses it to pick an agent engine.
    #[serde(default)]
    pub harness: AgentKind,
}

impl DevelopmentEnvironment {
    /// The built-in environment every other one is built on top of.
    ///
    /// Not stored in the settings file, because a name that cannot be edited or
    /// deleted is not a preference. Its setup command is empty, and empty means
    /// the snapshot is the base image plus the workspace packages plus the
    /// profile recipes, with nothing added: a bare environment costs one build
    /// and no packages of its own.
    pub fn base() -> Self {
        Self {
            name: BASE_NAME.to_owned(),
            setup_command: String::new(),
            harness: AgentKind::default(),
        }
    }

    /// Whether this is the built-in environment.
    pub fn is_base(&self) -> bool {
        is_base_name(&self.name)
    }

    /// The key two environment names are compared by.
    ///
    /// An environment name becomes a snapshot directory name (`coding-{name}`),
    /// and macOS filesystems are case-insensitive by default. Treating `rust`
    /// and `Rust` as distinct would let both pass validation and then share one
    /// snapshot directory, so distinct setup commands would silently reuse each
    /// other's environment. Comparison is case-insensitive to keep one
    /// environment per directory; the configured spelling is preserved
    /// everywhere it is stored, so a task still records the name as written.
    pub fn key(&self) -> String {
        self.name.to_ascii_lowercase()
    }

    /// Whether two environments are the same one, by that rule.
    pub fn same_name(&self, other: &Self) -> bool {
        self.key() == other.key()
    }

    pub fn validate(&self) -> Result<(), DevelopmentEnvironmentError> {
        let mut characters = self.name.chars();
        let valid_first = characters
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric());
        let valid_rest = characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
        if !valid_first || !valid_rest || self.name.len() > 64 {
            return Err(DevelopmentEnvironmentError(format!(
                "environment name {:?} must be 1-64 ASCII letters, numbers, hyphens, or underscores, and start with a letter or number",
                self.name
            )));
        }
        Ok(())
    }
}

/// Whether a name is the reserved one.
pub fn is_base_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(BASE_NAME)
}

#[derive(Debug)]
pub struct DevelopmentEnvironmentError(String);

impl fmt::Display for DevelopmentEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for DevelopmentEnvironmentError {}
