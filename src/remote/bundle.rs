//! What a client asks a remote to hold.
//!
//! A remote is a replica of a setup, so this is the setup: how to build a task, and
//! what to run it as. It travels as one document rather than a copy of the
//! configuration directory, because the parts that matter are not the parts a file
//! copy would carry — a client's own remotes are not a remote's business, and a
//! profile's files are credentials rather than preferences.
//!
//! Each profile carries when it was last changed, so a client can send the ones the
//! remote does not have instead of all of them. Profiles are the expensive half:
//! sending a configuration string again costs nothing, and sending a directory of
//! logins again does.

use std::{fs, io, path::Path, time::UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

/// Everything a remote needs to be this client, as far as running a task goes.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigBundle {
    /// The configuration file as written, so the remote holds the same environments.
    pub settings: String,
    /// The logins, one entry per profile, and only the ones being sent.
    pub profiles: Vec<ProfileBundle>,
}

/// One profile's files, and when the newest of them was changed.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileBundle {
    pub profile: String,
    /// The newest change among these files. A remote that already has this one, or a
    /// newer one, is not sent them again.
    pub edited_at_ms: i64,
    pub files: Vec<ProfileFile>,
}

/// One file inside a profile, at its path relative to the profile's directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProfileFile {
    pub path: String,
    /// Base64, because these are credential files and the bundle is JSON.
    pub contents: String,
}

/// What a remote says it holds for a profile, so a client can send only what is new.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileStamp {
    pub profile: String,
    pub edited_at_ms: i64,
}

/// What a remote holds, kept beside its configuration so a client can be told what
/// not to send again.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HeldConfig {
    pub profiles: Vec<ProfileStamp>,
}

impl HeldConfig {
    /// Reads what this remote holds, or nothing when it has been sent nothing yet.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes it beside the real record and moves it into place, so a reader never
    /// sees half of one.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let incoming = path.with_extension("json.incoming");
        fs::write(&incoming, serde_json::to_vec(self)?)?;
        fs::rename(&incoming, path)
    }
}

impl ConfigBundle {
    /// Reads this installation's setup, sending the profiles a remote is missing.
    ///
    /// `held` is what the remote said it has. A profile is sent when it is not there
    /// at all, or when the copy here is newer than the one there.
    pub fn of(
        settings: String,
        data_dir: &Path,
        profiles: &[&str],
        held: &[ProfileStamp],
    ) -> Result<Self, io::Error> {
        let mut sending = Vec::new();
        for profile in profiles {
            let bundle = ProfileBundle::of(data_dir, profile)?;
            let is_missing_or_older = held
                .iter()
                .find(|stamp| stamp.profile == *profile)
                .is_none_or(|stamp| stamp.edited_at_ms < bundle.edited_at_ms);
            if is_missing_or_older && !bundle.files.is_empty() {
                sending.push(bundle);
            }
        }
        Ok(Self {
            settings,
            profiles: sending,
        })
    }
}

impl ProfileBundle {
    /// Reads one profile's directory, or nothing when it has never been used.
    ///
    /// A profile nobody has logged into is a directory that does not exist, which is
    /// not an error: it means there is nothing to send.
    pub fn of(data_dir: &Path, profile: &str) -> Result<Self, io::Error> {
        let root = data_dir.join(profile).join("profile");
        let mut files = Vec::new();
        let mut edited_at_ms = 0;
        if root.is_dir() {
            collect(&root, &root, &mut files, &mut edited_at_ms)?;
        }
        Ok(Self {
            profile: profile.to_owned(),
            edited_at_ms,
            files,
        })
    }
}

/// Walks a profile, keeping every file and the newest time anything in it changed.
fn collect(
    root: &Path,
    directory: &Path,
    files: &mut Vec<ProfileFile>,
    edited_at_ms: &mut i64,
) -> Result<(), io::Error> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            collect(root, &path, files, edited_at_ms)?;
            continue;
        }
        // A symlink inside a profile points at something on the machine that wrote
        // it, which means nothing here. Only the files themselves travel.
        if meta.file_type().is_symlink() {
            continue;
        }
        if let Ok(modified) = meta.modified()
            && let Ok(since) = modified.duration_since(UNIX_EPOCH)
        {
            *edited_at_ms = (*edited_at_ms).max(since.as_millis() as i64);
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        files.push(ProfileFile {
            path: relative,
            contents: BASE64.encode(fs::read(&path)?),
        });
    }
    Ok(())
}
