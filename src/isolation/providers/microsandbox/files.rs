use std::{
    fs,
    io::{self},
    os::unix::fs::PermissionsExt,
    path::Path,
};

use microsandbox::sandbox::fs::{FsEntryKind, SandboxFsOps};

use crate::isolation::{FileTally, IsolationError};

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

pub(super) struct HostTree {
    pub(super) directories: Vec<String>,
    pub(super) files: Vec<HostFile>,
    pub(super) links: Vec<GuestLink>,
    pub(super) whole: FileTally,
}

pub(super) struct HostFile {
    pub(super) relative: String,
    pub(super) size: u64,
    pub(super) mode: u32,
}

pub(super) fn host_tree(source: &Path) -> Result<HostTree, IsolationError> {
    let mut tree = HostTree {
        directories: Vec::new(),
        files: Vec::new(),
        links: Vec::new(),
        whole: FileTally::default(),
    };
    let mut pending = vec![(source.to_path_buf(), String::new())];
    while let Some((directory, relative_directory)) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(io_error)?;
        for entry in entries {
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = if relative_directory.is_empty() {
                name.clone()
            } else {
                format!("{relative_directory}/{name}")
            };
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
            if metadata.is_symlink() {
                let target = fs::read_link(&path).map_err(io_error)?;
                tree.links.push(GuestLink {
                    relative,
                    target: target.to_string_lossy().into_owned(),
                });
            } else if metadata.is_dir() {
                tree.directories.push(relative.clone());
                pending.push((path, relative));
            } else {
                tree.whole.files += 1;
                tree.whole.bytes += metadata.len();
                tree.files.push(HostFile {
                    relative,
                    size: metadata.len(),
                    mode: metadata.permissions().mode(),
                });
            }
        }
    }
    Ok(tree)
}

pub(super) fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
}

pub(super) fn permissions(mode: u32) -> fs::Permissions {
    fs::Permissions::from_mode(mode)
}

pub(super) const COPY_CONCURRENCY: usize = 8;

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
            let entries = fs
                .list(&directory)
                .await
                .map_err(|error| IsolationError::new(error.to_string()))?;
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
        Ok(tree)
    }
}
