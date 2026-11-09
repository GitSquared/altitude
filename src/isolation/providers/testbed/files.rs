use std::{
    fs, io,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
};

use crate::isolation::{FileTally, FileTransferSink, IsolationError};

pub(super) fn copy_out(
    source: &Path,
    guest_root: &str,
    excluded: &[String],
    destination: &Path,
    progress: &dyn FileTransferSink,
) -> Result<FileTally, IsolationError> {
    let mut tally = FileTally::default();
    if !source.exists() {
        progress.moving(tally);
        return Ok(tally);
    }
    walk_out(
        source,
        Path::new(""),
        guest_root,
        excluded,
        destination,
        &mut tally,
    )?;
    progress.moving(tally);
    Ok(tally)
}

fn walk_out(
    entry: &Path,
    relative: &Path,
    guest_root: &str,
    excluded: &[String],
    destination: &Path,
    tally: &mut FileTally,
) -> Result<(), IsolationError> {
    let guest = join_guest(guest_root, relative);
    if excluded.iter().any(|path| under(&guest, path)) {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(entry).map_err(io_error)?;
    let target = destination.join(relative);
    if metadata.file_type().is_symlink() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let link = fs::read_link(entry).map_err(io_error)?;
        let _ = fs::remove_file(&target);
        symlink(link, &target).map_err(io_error)?;
        return Ok(());
    }
    if metadata.is_dir() {
        fs::create_dir_all(&target).map_err(io_error)?;
        let children = fs::read_dir(entry).map_err(io_error)?;
        for child in children {
            let child = child.map_err(io_error)?;
            let name = child.file_name();
            walk_out(
                &child.path(),
                &relative.join(name),
                guest_root,
                excluded,
                destination,
                tally,
            )?;
        }
        return Ok(());
    }
    if metadata.is_file() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        fs::copy(entry, &target).map_err(io_error)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).map_err(io_error)?;
        tally.files += 1;
        tally.bytes += metadata.len();
    }
    Ok(())
}

pub(super) fn copy_in(
    source: &Path,
    target: &Path,
    progress: &dyn FileTransferSink,
) -> Result<FileTally, IsolationError> {
    let mut tally = FileTally::default();
    let metadata = fs::symlink_metadata(source).map_err(io_error)?;
    if metadata.is_dir() {
        fs::create_dir_all(target).map_err(io_error)?;
        let children = fs::read_dir(source).map_err(io_error)?;
        for child in children {
            let child = child.map_err(io_error)?;
            walk_in(&child.path(), &target.join(child.file_name()), &mut tally)?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        fs::copy(source, target).map_err(io_error)?;
        tally.files += 1;
        tally.bytes += metadata.len();
    }
    progress.moving(tally);
    Ok(tally)
}

fn walk_in(entry: &Path, target: &Path, tally: &mut FileTally) -> Result<(), IsolationError> {
    let metadata = fs::symlink_metadata(entry).map_err(io_error)?;
    if metadata.file_type().is_symlink() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let link = fs::read_link(entry).map_err(io_error)?;
        let _ = fs::remove_file(target);
        symlink(link, target).map_err(io_error)?;
        return Ok(());
    }
    if metadata.is_dir() {
        fs::create_dir_all(target).map_err(io_error)?;
        let children = fs::read_dir(entry).map_err(io_error)?;
        for child in children {
            let child = child.map_err(io_error)?;
            walk_in(&child.path(), &target.join(child.file_name()), tally)?;
        }
        return Ok(());
    }
    if metadata.is_file() {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        fs::copy(entry, target).map_err(io_error)?;
        fs::set_permissions(target, fs::Permissions::from_mode(0o600)).map_err(io_error)?;
        tally.files += 1;
        tally.bytes += metadata.len();
    }
    Ok(())
}

pub(super) fn copy_file(source: &Path, target: &Path) -> Result<u64, IsolationError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    // A link where the file goes would have the write land on whatever it points
    // at, so the name is cleared first.
    let _ = fs::remove_file(target);
    let bytes = fs::copy(source, target).map_err(io_error)?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o600)).map_err(io_error)?;
    Ok(bytes)
}

pub(super) fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

fn join_guest(root: &str, relative: &Path) -> String {
    let root = root.trim_end_matches('/');
    let relative = relative.to_string_lossy();
    if relative.is_empty() {
        return root.to_owned();
    }
    format!("{root}/{}", relative.trim_start_matches('/'))
}

fn under(path: &str, parent: &str) -> bool {
    let parent = parent.trim_end_matches('/');
    path == parent || path.starts_with(&format!("{parent}/"))
}

fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
}
