//! Client settings and private profile snapshots exchanged with a remote.
//! Digests identify snapshots already received, including empty profiles that
//! remove credentials the client no longer holds.

use std::{fs, io, path::Path};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::tracker::TrackerKind;

/// Everything a remote needs to be this client, as far as running a task goes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigBundle {
    /// The configuration, so the remote holds the same environments.
    ///
    /// The part a remote can act on, not the file as written: what this interface
    /// does is no business of a machine running a task, and hashing it would sync a
    /// preference as though it were configuration.
    pub settings: String,
    /// The logins, one entry per profile, and only the ones being sent.
    pub profiles: Vec<ProfileBundle>,
}

/// A profile snapshot, including an empty snapshot when its files were removed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileBundle {
    pub profile: String,
    pub digest: String,
    pub files: Vec<ProfileFile>,
}

/// One file inside a profile, at its path relative to the profile's directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileFile {
    pub path: String,
    /// Base64, because these are credential files and the bundle is JSON.
    pub contents: String,
}

/// What a remote says it holds for a profile, so a client can send only what is new.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDigest {
    pub profile: String,
    pub digest: String,
}

/// What a remote holds, kept beside its configuration so a client can be told what
/// not to send again.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HeldConfig {
    pub profiles: Vec<ProfileDigest>,
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
    pub fn digest(&self) -> io::Result<String> {
        let profiles = self
            .profiles
            .iter()
            .map(|profile| (&profile.profile, &profile.digest))
            .collect::<Vec<_>>();
        Ok(
            blake3::hash(&serde_json::to_vec(&(&self.settings, profiles))?)
                .to_hex()
                .to_string(),
        )
    }

    pub fn retain_changed_profiles(&mut self, held: &[ProfileDigest]) {
        self.profiles.retain(|profile| {
            !held
                .iter()
                .any(|stamp| stamp.profile == profile.profile && stamp.digest == profile.digest)
        });
    }

    /// Reads this installation's setup, sending the profiles a remote is missing.
    ///
    /// `mounted` is the logins a task mounts. The tracker profiles travel with
    /// them but are never mounted: what they carry is the account a remote routes
    /// tracker work by, not something a capsule reads.
    ///
    /// `held` is what the remote said it has. A profile is sent when it is not there
    /// at all, or when its contents differ.
    /// `host` is the machine this bundle is for: only the projects it runs travel,
    /// so a remote never learns about work that belongs somewhere else.
    pub fn of(
        settings: &str,
        data_dir: &Path,
        mounted: &[&str],
        held: &[ProfileDigest],
        host: Option<&str>,
    ) -> Result<Self, io::Error> {
        let parsed: Settings = toml::from_str(settings).map_err(io::Error::other)?;
        let mut shared = parsed.shared();
        if let Some(host) = host {
            shared.retain_projects_for(host);
        }
        let settings = toml::to_string(&shared).map_err(io::Error::other)?;
        let profiles = mounted
            .iter()
            .copied()
            .chain(TrackerKind::ALL.iter().map(|kind| kind.profile()))
            .map(|profile| ProfileBundle::of(data_dir, profile))
            .collect::<io::Result<Vec<_>>>()?;
        let mut bundle = Self { settings, profiles };
        bundle.retain_changed_profiles(held);
        Ok(bundle)
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
        if root.is_dir() {
            collect(&root, &root, &mut files)?;
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let digest = blake3::hash(&serde_json::to_vec(&files)?)
            .to_hex()
            .to_string();
        Ok(Self {
            profile: profile.to_owned(),
            digest,
            files,
        })
    }
}

fn collect(root: &Path, directory: &Path, files: &mut Vec<ProfileFile>) -> Result<(), io::Error> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            collect(root, &path, files)?;
            continue;
        }
        // A symlink inside a profile points at something on the machine that wrote
        // it, which means nothing here. Only the files themselves travel.
        if meta.file_type().is_symlink() {
            continue;
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
