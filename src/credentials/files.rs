//! Where each profile's CLI writes its sign-in.
//!
//! A file name is not a credential: Altitude reads presence and size, never contents.
//! Naming them here is what lets a saved login be recognised without a sandbox, and
//! removed without touching anything else the profile keeps.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// The login files a profile's CLI writes, relative to its host directory.
///
/// Only a sign-in: settings, models, managed MCP files, and environment setup are not
/// credentials and stay. A profile absent here has no login Altitude knows how to
/// recognise or remove.
pub(super) fn login_files(profile: &str) -> Option<&'static [&'static str]> {
    match profile {
        // Pi keeps one credential per provider here, and nothing else.
        "agents/pi" => Some(&["auth.json"]),
        // Claude Code's own login file; its settings and MCP files stay.
        "agents/claude" => Some(&[".credentials.json"]),
        // The GitHub CLI's host sign-in. The git identity, SSH keys, and signing keys
        // are separate material and are deliberately not touched.
        "vcs/github" => Some(&["gh/hosts.yml"]),
        _ => None,
    }
}

/// Whether a profile has a login file on disk that a task would mount.
///
/// The same question a profile's own check asks of the file it keeps its sign-in in,
/// answered without a sandbox. A file here says a login is saved, not that the CLI
/// would still accept it.
pub(super) fn has_saved_login(directory: &Path, profile: &str) -> bool {
    login_files(profile).is_some_and(|files| {
        files
            .iter()
            .any(|file| fs::metadata(directory.join(file)).is_ok_and(|metadata| metadata.len() > 0))
    })
}

/// Remove a profile's saved login, leaving everything else in it.
pub(super) fn forget(directory: &Path, profile: &str) -> Result<(), String> {
    let files = login_files(profile)
        .ok_or_else(|| format!("no saved login is known for the {profile} profile"))?;
    for file in files {
        remove(directory.join(*file))?;
    }
    Ok(())
}

fn remove(path: PathBuf) -> Result<(), String> {
    match fs::remove_file(&path) {
        // Already signed out is the state the operator asked for.
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not remove the saved login at {}: {error}",
            path.display()
        )),
    }
}
