//! What the node role reads.
//!
//! Its own file rather than the client's. A node has no default environment and
//! no task prompt; a client has no listening address. Sharing one file would
//! mean each role carrying fields it cannot interpret.

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use serde::{Deserialize, Serialize};

use super::SettingsError;

/// The address a node listens on when nobody has said otherwise.
///
/// 25848 is unassigned, which is all a port has to be, and it is one number to
/// remember: on a phone keypad `ALTIT` is 25848. TCP and UDP both use it, so an
/// operator has a single address to make reachable and a client has a single
/// thing to be told.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:25848";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ServeSettings {
    /// `address:port`, UDP.
    pub listen: String,
}

impl Default for ServeSettings {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.to_owned(),
        }
    }
}

impl ServeSettings {
    /// Reads the settings, writing a default file first if there is none.
    ///
    /// Written rather than assumed so that a fresh node leaves something to edit
    /// and something to point at, instead of behaviour that lives only in a
    /// binary's defaults.
    pub fn load_or_create(path: &Path) -> Result<Self, SettingsError> {
        match fs::read_to_string(path) {
            Ok(source) => toml::from_str(&source).map_err(SettingsError::Parse),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let settings = Self::default();
                let source = toml::to_string_pretty(&settings).map_err(SettingsError::Serialize)?;
                write_new(path, &source)?;
                Ok(settings)
            }
            Err(error) => Err(SettingsError::Io(error)),
        }
    }
}

/// Writes a file only the owner can read, refusing to replace one that exists.
fn write_new(path: &Path, contents: &str) -> Result<(), SettingsError> {
    let directory = path.parent().expect("a settings path has a parent");
    fs::create_dir_all(directory).map_err(SettingsError::Io)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(SettingsError::Io)?;
    file.write_all(contents.as_bytes())
        .map_err(SettingsError::Io)?;
    file.sync_all().map_err(SettingsError::Io)?;
    Ok(())
}
