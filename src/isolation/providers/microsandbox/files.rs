use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

use microsandbox::sandbox::fs::{
    FsEntry, FsEntryKind, FsHandle, FsOpenOptions, FsSetAttrs, SandboxFsOps,
};
use tar::{Archive, EntryType};

use crate::isolation::{FileTally, IsolationError};

// What a tree is, whether it was read out of a sandbox or out of an archive. The two
// are held against each other, so they are the same words: a tree is its directories,
// its files with the size and permissions they have, and its links with where they go.
pub(super) struct Tree {
    pub(super) directories: Vec<String>,
    pub(super) files: Vec<TreeFile>,
    pub(super) links: Vec<TreeLink>,
    pub(super) whole: FileTally,
}

pub(super) struct TreeFile {
    pub(super) relative: String,
    pub(super) size: u64,
    pub(super) mode: u32,
}

pub(super) struct TreeLink {
    pub(super) relative: String,
    pub(super) target: String,
}

pub(super) fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
}

// A guest retires a read as soon as it finishes, and a window refill that arrives
// after that cancels the whole read as an unknown session. A refill is only due
// once a stream has spent half of its 8 MiB credit, so a copy asks for less than
// that at a time and never has one to send.
pub(super) const COPY_SLICE_BYTES: u64 = 3 * 1024 * 1024;

pub(super) async fn walk(
    fs: &SandboxFsOps<'_>,
    root: &str,
    excluded: &[String],
) -> Result<Tree, IsolationError> {
    let mut tree = Tree {
        directories: Vec::new(),
        files: Vec::new(),
        links: Vec::new(),
        whole: FileTally::default(),
    };
    let mut pending = vec![(root.trim_end_matches('/').to_owned(), String::new())];
    while let Some((directory, relative_directory)) = pending.pop() {
        // Opened and read through handles on the one connection the sandbox gave us:
        // a listing per directory by path would dial the agent again for each one.
        let handle = fs
            .open_dir(&directory)
            .await
            .map_err(|error| IsolationError::new(format!("opening {directory}: {error}")))?;
        let mut listed = Ok(());
        loop {
            let entries = match fs.read_dir_handle(handle, None).await {
                Ok(entries) => entries,
                Err(error) => {
                    listed = Err(IsolationError::new(format!("reading {directory}: {error}")));
                    break;
                }
            };
            if entries.is_empty() {
                break;
            }
            for entry in entries {
                let name = entry.path.rsplit('/').next().unwrap_or_default().to_owned();
                if name.is_empty() || name == "." || name == ".." {
                    continue;
                }
                let relative = if relative_directory.is_empty() {
                    name
                } else {
                    format!("{relative_directory}/{name}")
                };
                if excluded
                    .iter()
                    .any(|left| entry.path == *left || entry.path.starts_with(&format!("{left}/")))
                {
                    continue;
                }
                match entry.kind {
                    FsEntryKind::Directory => {
                        tree.directories.push(relative.clone());
                        pending.push((entry.path, relative));
                    }
                    FsEntryKind::File => {
                        tree.whole.files += 1;
                        tree.whole.bytes += entry.size;
                        tree.files.push(TreeFile {
                            relative,
                            size: entry.size,
                            mode: entry.mode,
                        });
                    }
                    FsEntryKind::Symlink => {
                        let target = fs
                            .read_link(&entry.path)
                            .await
                            .map_err(|error| IsolationError::new(error.to_string()))?;
                        tree.links.push(TreeLink { relative, target });
                    }
                    FsEntryKind::Other => {}
                }
            }
        }
        if let Err(error) = fs.close_handle(handle).await {
            tracing::warn!(%error, directory, "could not close a directory handle");
        }
        listed?;
    }
    Ok(tree)
}

pub(super) fn read_archive(source: &Path) -> Result<Tree, IsolationError> {
    let mut archive = Archive::new(File::open(source).map_err(io_error)?);
    let mut tree = Tree {
        directories: Vec::new(),
        files: Vec::new(),
        links: Vec::new(),
        whole: FileTally::default(),
    };
    for entry in archive.entries().map_err(archive_error)? {
        let entry = entry.map_err(archive_error)?;
        let named = entry
            .path()
            .map_err(archive_error)?
            .to_string_lossy()
            .into_owned();
        // A tar of `.` names everything under a leading `./`, names the root itself
        // as `.`, and names a directory with a trailing slash; what is carried are
        // the names below the root, which is what the walk says too.
        let relative = named
            .strip_prefix("./")
            .unwrap_or(&named)
            .trim_end_matches('/')
            .to_owned();
        if relative.is_empty() || relative == "." {
            continue;
        }
        // What is carried is a tree, so an entry that climbs out of it is not one. A
        // link's own target may point anywhere; its name may not.
        if relative.starts_with('/') || relative.split('/').any(|part| part == "..") {
            return Err(IsolationError::new(format!(
                "archive entry escapes the tree: {relative}"
            )));
        }
        match entry.header().entry_type() {
            EntryType::Directory => tree.directories.push(relative),
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(archive_error)?
                    .map(|target| target.to_string_lossy().into_owned())
                    .unwrap_or_default();
                tree.links.push(TreeLink { relative, target });
            }
            EntryType::Regular | EntryType::Continuous => {
                let size = entry.size();
                tree.whole.files += 1;
                tree.whole.bytes += size;
                tree.files.push(TreeFile {
                    relative,
                    size,
                    mode: entry.header().mode().map_err(archive_error)?,
                });
            }
            _ => {}
        }
    }
    Ok(tree)
}

// The first way the two disagree, if they do. What the sandbox packed is two
// descriptions of one tree, so either holding more than the other is a
// disagreement. A transfer that quietly loses a file is the one failure it must
// never have: it looks like a task that did less.
pub(super) fn disagreement(carried: &Tree, held: &Tree) -> Result<(), IsolationError> {
    let carried = describe(carried);
    let held = describe(held);
    for (name, description) in &carried {
        match held.get(name) {
            // A directory is the same directory wherever it is met.
            Some(here) if name.ends_with('/') || here == description => {}
            Some(here) => {
                return Err(IsolationError::new(format!(
                    "{name} is {here} here and {description} in the archive"
                )));
            }
            None => {
                return Err(IsolationError::new(format!(
                    "{name} is {description} in the archive and not here"
                )));
            }
        }
    }
    for (name, description) in &held {
        if !carried.contains_key(name) {
            return Err(IsolationError::new(format!(
                "{name} is {description} here and not in the archive"
            )));
        }
    }
    Ok(())
}

fn describe(tree: &Tree) -> BTreeMap<String, String> {
    let mut described = BTreeMap::new();
    for directory in &tree.directories {
        described.insert(format!("{directory}/"), "a directory".to_owned());
    }
    for file in &tree.files {
        // A guest stat reports the file's type in the mode as well; a tar header
        // records the permission bits. What is compared is the permissions.
        described.insert(
            file.relative.clone(),
            format!("{} bytes, mode {:o}", file.size, file.mode & 0o7777),
        );
    }
    for link in &tree.links {
        described.insert(link.relative.clone(), format!("a link to {}", link.target));
    }
    described
}

// Everything the archive carries, looked for where it should be. A checkout may hold
// more than the capsule — its clone brought files of its own — never less, and never
// something else under a name the capsule names. One listing per directory the
// capsule names, so a cache mounted inside the checkout is never descended into.
pub(super) async fn confirm_arrival(
    fs: &SandboxFsOps<'_>,
    root: &str,
    carried: &Tree,
) -> Result<(), IsolationError> {
    let root = root.trim_end_matches('/');
    let carried = describe(carried);
    // Shallowest first, so a tree that is not there at all is reported at the root
    // rather than at whichever directory happened to be looked at first.
    let mut by_directory: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    for name in carried.keys() {
        let parent = name
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or_default();
        by_directory.entry(parent).or_default().push(name);
    }
    for (parent, names) in by_directory {
        let directory = if parent.is_empty() {
            root.to_owned()
        } else {
            format!("{root}/{parent}")
        };
        let present = match list(fs, &directory).await {
            Ok(present) => present,
            // Every other directory was seen in its parent's listing first, so the
            // one whose absence is ours to explain is the root.
            Err(error) if parent.is_empty() => {
                return Err(IsolationError::new(format!(
                    "{directory} is not there after unpacking: {error}"
                )));
            }
            Err(error) => return Err(error),
        };
        for name in names {
            let leaf = name
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(name);
            match present.get(leaf) {
                Some(here) if here == &carried[name] => {}
                Some(here) => {
                    return Err(IsolationError::new(format!(
                        "{directory}/{leaf} was unpacked as {here} and the archive carries {}",
                        carried[name]
                    )));
                }
                None => {
                    return Err(IsolationError::new(format!(
                        "{directory}/{leaf} was unpacked and is not there"
                    )));
                }
            }
        }
    }
    Ok(())
}

// What one directory holds now, described the way an archive describes it.
async fn list(
    fs: &SandboxFsOps<'_>,
    directory: &str,
) -> Result<BTreeMap<String, String>, IsolationError> {
    let handle = fs
        .open_dir(directory)
        .await
        .map_err(|error| IsolationError::new(format!("opening {directory}: {error}")))?;
    let mut present = BTreeMap::new();
    let mut listed = Ok(());
    loop {
        let entries = match fs.read_dir_handle(handle, None).await {
            Ok(entries) => entries,
            Err(error) => {
                listed = Err(IsolationError::new(format!("reading {directory}: {error}")));
                break;
            }
        };
        if entries.is_empty() {
            break;
        }
        for entry in entries {
            let name = entry.path.rsplit('/').next().unwrap_or_default().to_owned();
            present.insert(name, describe_entry(fs, &entry).await?);
        }
    }
    if let Err(error) = fs.close_handle(handle).await {
        tracing::warn!(%error, directory, "could not close a directory handle");
    }
    listed?;
    Ok(present)
}

async fn describe_entry(fs: &SandboxFsOps<'_>, entry: &FsEntry) -> Result<String, IsolationError> {
    Ok(match entry.kind {
        FsEntryKind::Directory => "a directory".to_owned(),
        FsEntryKind::File => format!("{} bytes, mode {:o}", entry.size, entry.mode & 0o7777),
        FsEntryKind::Symlink => format!(
            "a link to {}",
            fs.read_link(&entry.path)
                .await
                .map_err(|error| IsolationError::new(error.to_string()))?
        ),
        FsEntryKind::Other => "something else".to_owned(),
    })
}

// One file that is not part of a tree, carried out on its own: a task's transcript.
pub(super) async fn carry_out(
    fs: &SandboxFsOps<'_>,
    guest: &str,
    host: &Path,
    mode: u32,
) -> Result<u64, IsolationError> {
    if fs::symlink_metadata(host).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(IsolationError::new(format!(
            "refusing to write through a symlink: {}",
            host.display()
        )));
    }
    let handle = open_guest_file(
        fs,
        guest,
        FsOpenOptions {
            read: true,
            ..FsOpenOptions::default()
        },
    )
    .await?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(host)
        .map_err(io_error)?;
    let mut offset = 0u64;
    let copied = loop {
        let chunk = match fs.read_handle(handle, offset, Some(COPY_SLICE_BYTES)).await {
            Ok(chunk) => chunk,
            Err(error) => break Err(IsolationError::new(error.to_string())),
        };
        if let Err(error) = file.write_all(&chunk) {
            break Err(io_error(error));
        }
        offset += chunk.len() as u64;
        if (chunk.len() as u64) < COPY_SLICE_BYTES {
            break Ok(offset);
        }
    };
    close_guest_handle(fs, handle, guest).await;
    let written = copied?;
    drop(file);
    fs::set_permissions(host, fs::Permissions::from_mode(mode)).map_err(io_error)?;
    Ok(written)
}

// One file that is not part of a tree, put in on its own: a task's transcript.
pub(super) async fn carry_in(
    fs: &SandboxFsOps<'_>,
    source: &Path,
    guest: &str,
    mode: u32,
) -> Result<u64, IsolationError> {
    let file = File::open(source).map_err(io_error)?;
    let size = file.metadata().map_err(io_error)?.len();
    // Putting a file in replaces what is at that name: going through a link someone
    // left there would edit a file this one has nothing to do with.
    let _ = fs.remove(guest).await;
    let handle = open_guest_file(
        fs,
        guest,
        FsOpenOptions {
            write: true,
            create: true,
            truncate: true,
            ..FsOpenOptions::default()
        },
    )
    .await?;
    let mut offset = 0u64;
    // Sized to this file rather than to the slice: a checkout of small files would
    // otherwise allocate and clear a slice-sized buffer once per file.
    let mut buffer = vec![0u8; COPY_SLICE_BYTES.min(size) as usize];
    let mut written = Ok(0);
    while offset < size {
        let len = COPY_SLICE_BYTES.min(size - offset) as usize;
        if let Err(error) = file.read_exact_at(&mut buffer[..len], offset) {
            written = Err(io_error(error));
            break;
        }
        if let Err(error) = fs.write_handle(handle, offset, &buffer[..len]).await {
            written = Err(IsolationError::new(error.to_string()));
            break;
        }
        offset += len as u64;
        written = Ok(offset);
    }
    settle_written_file(fs, handle, guest, mode, written).await
}

async fn open_guest_file(
    fs: &SandboxFsOps<'_>,
    guest: &str,
    options: FsOpenOptions,
) -> Result<FsHandle, IsolationError> {
    fs.open_file(guest, options)
        .await
        .map_err(|error| IsolationError::new(format!("opening {guest}: {error}")))
}

// Creation goes through the guest's umask, so the bits a task recorded are set
// explicitly; an executable left non-executable is a real loss. A handle that will
// not close is worth a line in the log, not a failed transfer: the bytes are in.
async fn settle_written_file(
    fs: &SandboxFsOps<'_>,
    handle: FsHandle,
    guest: &str,
    mode: u32,
    written: Result<u64, IsolationError>,
) -> Result<u64, IsolationError> {
    if written.is_ok()
        && let Err(error) = fs
            .fset_stat(
                handle,
                FsSetAttrs {
                    mode: Some(mode),
                    ..FsSetAttrs::default()
                },
            )
            .await
    {
        tracing::warn!(guest, %error, "could not restore a file's mode");
    }
    close_guest_handle(fs, handle, guest).await;
    written
}

async fn close_guest_handle(fs: &SandboxFsOps<'_>, handle: FsHandle, guest: &str) {
    if let Err(error) = fs.close_handle(handle).await {
        tracing::warn!(%error, guest, "could not close a file handle in the sandbox");
    }
}

fn archive_error(error: io::Error) -> IsolationError {
    IsolationError::new(format!("archive: {error}"))
}
