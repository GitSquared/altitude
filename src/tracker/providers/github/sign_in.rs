use std::{
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::tracker::TrackerError;

const GRANT_FILE: &str = "grant.json";

// The sign-in is the GitHub CLI's, read once and kept in the profile: the profile is
// what travels to a remote, and a remote's own `gh` is not this operator's. altitude
// already asks for `gh` to be signed in for version control, so connecting the
// tracker asks nothing new of the operator.
#[derive(Serialize, Deserialize)]
pub(super) struct Grant {
    pub(super) token: String,
}

impl Grant {
    pub(super) fn from_cli() -> Result<Self, TrackerError> {
        let output = Command::new("gh")
            .args(["auth", "token", "--hostname", "github.com"])
            .output()
            .map_err(|_| {
                TrackerError::new(
                    "the GitHub CLI is not installed here; install gh and run `gh auth login`",
                )
            })?;
        let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || token.is_empty() {
            return Err(TrackerError::new(
                "the GitHub CLI is not signed in; run `gh auth login`, then connect again",
            ));
        }
        Ok(Self { token })
    }

    pub(super) fn load(profile: &Path) -> Result<Self, TrackerError> {
        let text = fs::read_to_string(path(profile))
            .map_err(|_| TrackerError::new("Connect GitHub in Settings → Task tracker"))?;
        serde_json::from_str(&text)
            .map_err(|_| TrackerError::new("the saved GitHub sign-in could not be read"))
    }

    pub(super) fn save(&self, profile: &Path) -> Result<(), TrackerError> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|_| TrackerError::new("could not encode the GitHub sign-in"))?;
        write_private(&path(profile), &text)
            .map_err(|_| TrackerError::new("could not save the GitHub sign-in"))
    }
}

pub(super) fn has_grant(profile: &Path) -> bool {
    path(profile).is_file()
}

pub(super) fn grant_changed_at(profile: &Path) -> Option<SystemTime> {
    fs::metadata(path(profile))
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

fn path(profile: &Path) -> PathBuf {
    profile.join(GRANT_FILE)
}

pub(super) fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let temporary = path.with_file_name(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
