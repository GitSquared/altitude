//! The keypair that names an installation, and the file plumbing for machine-local
//! state.
//!
//! The path is always the caller's: nothing here picks a default location or
//! keeps global state, so the client and the serve role can each hold their own
//! identity.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use iroh::{EndpointId, SecretKey};

use super::LinkResult;

// ------------------------------------------------------------------ identity

/// An installation's transport identity: an Ed25519 keypair on disk.
///
/// The caller supplies the path. There is no default location and no global
/// state, so the client and the serve role can each hold their own.
pub struct InstallationKey {
    secret_key: SecretKey,
}

impl InstallationKey {
    /// Loads the keypair at `path`, generating and writing one if absent.
    ///
    /// The file holds the 32 secret bytes, base64, on one line. On unix it is
    /// created with mode 0600.
    pub fn load_or_create(path: impl Into<PathBuf>) -> LinkResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let secret_key = match fs::read_to_string(&path) {
            Ok(text) => parse_secret(&text)?,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let secret_key = SecretKey::generate();
                write_secret(&path, &secret_key)?;
                secret_key
            }
            Err(err) => return Err(err.into()),
        };
        Ok(Self { secret_key })
    }

    /// The public key other peers dial.
    pub fn id(&self) -> EndpointId {
        self.secret_key.public()
    }

    /// The key itself, for building an endpoint.
    pub fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }
}

fn parse_secret(text: &str) -> LinkResult<SecretKey> {
    let decoded = BASE64.decode(text.trim())?;
    let bytes: [u8; 32] = decoded
        .try_into()
        .map_err(|_| "secret key file must hold exactly 32 bytes")?;
    Ok(SecretKey::from_bytes(&bytes))
}

fn write_secret(path: &Path, secret_key: &SecretKey) -> LinkResult<()> {
    let text = format!("{}\n", BASE64.encode(secret_key.to_bytes()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // `create_new` also means two processes racing to create the same
        // identity cannot silently overwrite each other's key.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    fs::write(path, text)?;
    Ok(())
}

/// Writes a file only the owner can read, replaced atomically.
///
/// The rename means a reader sees either the old record or the new one, never a
/// half-written list, and it carries the temporary file's mode across.
pub(super) fn write_private(path: &Path, text: &str) -> LinkResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> LinkResult<()> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        fs::write(&temporary, text)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
