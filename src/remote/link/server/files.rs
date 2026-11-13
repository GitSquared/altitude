use super::*;

pub(super) fn arriving_accounts(
    decoded: &[(PathBuf, Vec<u8>)],
) -> Vec<(TrackerKind, Option<TrackerAccount>)> {
    let logins: Vec<(String, &[u8])> = decoded
        .iter()
        .filter_map(|(path, bytes)| {
            if path.file_name()? != ACCOUNT_FILE {
                return None;
            }
            // `trackers/<provider>/<operator>/profile/identity.json` names the
            // login two directories up.
            let login = path.parent()?.parent()?;
            Some((login.to_string_lossy().into_owned(), bytes.as_slice()))
        })
        .collect();
    TrackerKind::ALL
        .iter()
        .map(|provider| {
            let account = logins
                .iter()
                .find(|(login, _)| Service::from_id(login) == Some(Service::Tracker(*provider)))
                .and_then(|(_, bytes)| serde_json::from_slice::<TrackerAccount>(bytes).ok())
                .filter(|account| account.provider == *provider);
            (*provider, account)
        })
        .collect()
}

pub(super) fn is_allowed_profile(profile: &str) -> bool {
    // A pushed profile is a credential id, and the id says which service it
    // belongs to. The path checks below keep it confined; this keeps it a
    // service this build knows.
    Service::from_id(profile).is_some()
}

// ------------------------------------------------------------- the server side

pub(super) fn remove_missing_profile_files(
    directory: &Dir,
    relative: &Path,
    wanted: &HashSet<PathBuf>,
) -> io::Result<()> {
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let path = relative.join(&name);
        if entry.file_type()?.is_dir() {
            remove_missing_profile_files(&directory.open_dir(&name)?, &path, wanted)?;
        } else if !wanted.contains(&path) {
            directory.remove_file(&name)?;
        }
    }
    Ok(())
}

pub(super) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        make_private(parent)?;
    }
    let incoming = path.with_extension("incoming");
    {
        let mut file = fs::File::create(&incoming)?;
        make_private(&incoming)?;
        file.write_all(bytes)?;
    }
    fs::rename(&incoming, path)
}

#[cfg(unix)]
fn make_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn make_private(_path: &Path) -> io::Result<()> {
    Ok(())
}
