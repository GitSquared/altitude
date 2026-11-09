use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::credentials::Service;

pub(super) fn login_files(profile: &str) -> Option<&'static [&'static str]> {
    // A named identity has a suffix after the service it belongs to, but it
    // keeps that service's login file in its own directory.
    match Service::from_id(profile)? {
        Service::Agent(kind) => Some(kind.login_files()),
        Service::VersionControl(kind) => Some(kind.login_files()),
        // A tracker's session is host-side, not a mounted profile.
        Service::Tracker(_) => None,
    }
}

pub(super) fn has_saved_login(directory: &Path, profile: &str) -> bool {
    login_files(profile).is_some_and(|files| {
        files
            .iter()
            .any(|file| fs::metadata(directory.join(file)).is_ok_and(|metadata| metadata.len() > 0))
    })
}

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
