use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

use futures_util::{StreamExt, stream};
use microsandbox::sandbox::fs::{FsEntryKind, FsHandle, FsOpenOptions, FsSetAttrs, SandboxFsOps};
use tar::{Archive, Builder, EntryType, Header};

use crate::isolation::{FileTally, FileTransferSink, IsolationError};

use super::MicrosandboxIsolation;

pub(super) struct GuestTree {
    pub(super) directories: Vec<String>,
    pub(super) files: Vec<GuestFile>,
    pub(super) links: Vec<GuestLink>,
    pub(super) whole: FileTally,
}

pub(super) struct GuestFile {
    pub(super) guest: String,
    pub(super) relative: String,
    pub(super) size: u64,
    pub(super) mode: u32,
}

pub(super) struct GuestLink {
    pub(super) relative: String,
    pub(super) target: String,
}

// What an archive holds, read as offsets rather than as a stream: a file's bytes
// can then be written to or read from the archive directly, so a tree is neither
// assembled nor unpacked on a host disk.
pub(super) struct ArchivedTree {
    pub(super) directories: Vec<String>,
    pub(super) files: Vec<ArchivedFile>,
    pub(super) links: Vec<GuestLink>,
    pub(super) whole: FileTally,
}

pub(super) struct ArchivedFile {
    pub(super) relative: String,
    pub(super) offset: u64,
    pub(super) size: u64,
    pub(super) mode: u32,
}

pub(super) fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
}

pub(super) fn permissions(mode: u32) -> fs::Permissions {
    fs::Permissions::from_mode(mode)
}

pub(super) const COPY_CONCURRENCY: usize = 8;

// A guest retires a read as soon as it finishes, and a window refill that arrives
// after that cancels the whole read as an unknown session. A refill is only due
// once a stream has spent half of its 8 MiB credit, so a copy asks for less than
// that at a time and never has one to send.
pub(super) const COPY_SLICE_BYTES: u64 = 3 * 1024 * 1024;

// A tree's headers are written first, which is what gives every file its offset,
// and the bytes go into those offsets afterwards, several files at a time. The
// space a header reserves for its data is left as a hole, so reserving it costs
// nothing.
struct Holes<'a> {
    file: &'a File,
    position: u64,
}

impl Write for Holes<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.iter().any(|byte| *byte != 0) {
            self.file.write_all_at(buffer, self.position)?;
        }
        self.position += buffer.len() as u64;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Zeroes(u64);

impl Read for Zeroes {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let len = self.0.min(buffer.len() as u64) as usize;
        self.0 -= len as u64;
        buffer[..len].fill(0);
        Ok(len)
    }
}

pub(super) async fn write_archive(
    fs: &SandboxFsOps<'_>,
    tree: &GuestTree,
    destination: &Path,
    progress: &dyn FileTransferSink,
) -> Result<(), IsolationError> {
    let archive = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(destination)
        .map_err(io_error)?;
    let mut holes = Holes {
        file: &archive,
        position: 0,
    };
    {
        let mut builder = Builder::new(&mut holes);
        // A tree is its modes and its links as much as its bytes, and an archive
        // carries all three in one thing to move. Nothing here is dated: the same
        // tree is the same archive, which is what a digest is worth reading.
        for directory in &tree.directories {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            header.set_mtime(0);
            builder
                .append_data(&mut header, directory, io::empty())
                .map_err(archive_error)?;
        }
        for file in &tree.files {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_mode(file.mode);
            header.set_size(file.size);
            header.set_mtime(0);
            builder
                .append_data(&mut header, &file.relative, Zeroes(file.size))
                .map_err(archive_error)?;
        }
        for link in &tree.links {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            header.set_mtime(0);
            header.set_link_name(&link.target).map_err(archive_error)?;
            builder
                .append_data(&mut header, &link.relative, io::empty())
                .map_err(archive_error)?;
        }
        builder.finish().map_err(archive_error)?;
    }
    archive.set_len(holes.position).map_err(io_error)?;
    let layout = read_archive(destination)?;
    // Read back from the archive rather than recomputed: one place decides where a
    // file's bytes sit, so the side that fills them and the side that lifts them
    // cannot disagree.
    let guest_of: HashMap<&str, &str> = tree
        .files
        .iter()
        .map(|file| (file.relative.as_str(), file.guest.as_str()))
        .collect();
    let copying = layout.files.into_iter().map(|archived| {
        let guest = guest_of
            .get(archived.relative.as_str())
            .copied()
            .unwrap_or_default();
        let archive = &archive;
        async move {
            copy_guest_into_archive(fs, guest, &archived, archive)
                .await
                .map_err(|error| IsolationError::new(format!("carrying out {guest}: {error}")))?;
            progress.moved(FileTally {
                files: 1,
                bytes: archived.size,
            });
            Ok::<(), IsolationError>(())
        }
    });
    let mut copying = stream::iter(copying).buffer_unordered(COPY_CONCURRENCY);
    while let Some(result) = copying.next().await {
        result?;
    }
    Ok(())
}

pub(super) fn read_archive(source: &Path) -> Result<ArchivedTree, IsolationError> {
    let mut archive = Archive::new(File::open(source).map_err(io_error)?);
    let mut tree = ArchivedTree {
        directories: Vec::new(),
        files: Vec::new(),
        links: Vec::new(),
        whole: FileTally::default(),
    };
    for entry in archive.entries().map_err(archive_error)? {
        let entry = entry.map_err(archive_error)?;
        let relative = entry
            .path()
            .map_err(archive_error)?
            .to_string_lossy()
            .into_owned();
        let mode = entry.header().mode().map_err(archive_error)?;
        // What is carried is a tree, so an entry that climbs out of it is not one.
        // A link's own target may point anywhere; its name may not.
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
                tree.links.push(GuestLink { relative, target });
            }
            EntryType::Regular | EntryType::Continuous => {
                let size = entry.size();
                tree.whole.files += 1;
                tree.whole.bytes += size;
                tree.files.push(ArchivedFile {
                    relative,
                    offset: entry.raw_file_position(),
                    size,
                    mode,
                });
            }
            _ => {}
        }
    }
    Ok(tree)
}

async fn copy_guest_into_archive(
    fs: &SandboxFsOps<'_>,
    guest: &str,
    archived: &ArchivedFile,
    archive: &File,
) -> Result<(), IsolationError> {
    let handle = open_guest_file(
        fs,
        guest,
        FsOpenOptions {
            read: true,
            ..FsOpenOptions::default()
        },
    )
    .await?;
    let mut offset = 0u64;
    let mut copied = Ok(());
    while offset < archived.size {
        let len = COPY_SLICE_BYTES.min(archived.size - offset);
        let chunk = match fs.read_handle(handle, offset, Some(len)).await {
            Ok(chunk) => chunk,
            Err(error) => {
                copied = Err(IsolationError::new(error.to_string()));
                break;
            }
        };
        // The archive already says how long this file is, so a file that answers
        // with less has changed underneath the transfer and is worth refusing.
        if chunk.len() as u64 != len {
            copied = Err(IsolationError::new(format!(
                "the file changed while it was being carried out: asked for {len} bytes at {offset}, read {}",
                chunk.len()
            )));
            break;
        }
        if let Err(error) = archive.write_all_at(&chunk, archived.offset + offset) {
            copied = Err(io_error(error));
            break;
        }
        offset += len;
    }
    close_guest_handle(fs, handle, guest).await;
    copied
}

pub(super) async fn copy_archive_into_guest(
    fs: &SandboxFsOps<'_>,
    archive: &File,
    archived: &ArchivedFile,
    guest: &str,
) -> Result<(), IsolationError> {
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
    let mut buffer = vec![0u8; COPY_SLICE_BYTES.min(archived.size) as usize];
    let mut written = Ok(archived.size);
    while offset < archived.size {
        let len = COPY_SLICE_BYTES.min(archived.size - offset) as usize;
        if let Err(error) = archive.read_exact_at(&mut buffer[..len], archived.offset + offset) {
            written = Err(io_error(error));
            break;
        }
        if let Err(error) = fs.write_handle(handle, offset, &buffer[..len]).await {
            written = Err(IsolationError::new(error.to_string()));
            break;
        }
        offset += len as u64;
    }
    settle_written_file(fs, handle, guest, archived.mode, written)
        .await
        .map(|_| ())
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
    fs::set_permissions(host, permissions(mode)).map_err(io_error)?;
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

impl MicrosandboxIsolation {
    pub(super) async fn walk_guest(
        &self,
        fs: &SandboxFsOps<'_>,
        root: &str,
        excluded: &[String],
    ) -> Result<GuestTree, IsolationError> {
        let mut tree = GuestTree {
            directories: Vec::new(),
            files: Vec::new(),
            links: Vec::new(),
            whole: FileTally::default(),
        };
        let mut pending = vec![(root.trim_end_matches('/').to_owned(), String::new())];
        while let Some((directory, relative_directory)) = pending.pop() {
            // Opened and read through handles on the one connection the sandbox
            // gave us: a listing per directory by path would dial the agent again
            // for each one, which is most of what made a large checkout slow.
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
                    if excluded.iter().any(|left| {
                        entry.path == *left || entry.path.starts_with(&format!("{left}/"))
                    }) {
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
                            tree.files.push(GuestFile {
                                guest: entry.path,
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
                            tree.links.push(GuestLink { relative, target });
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
}
