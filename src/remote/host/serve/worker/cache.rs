use std::{
    fs,
    path::Path,
    time::{Instant, SystemTime},
};

use tar::{Archive, Builder};
use tempfile::NamedTempFile;
use tokio::task::spawn_blocking;

use crate::remote::layers::ArchiveKind;

use super::WorkerService;

// A cache is restored at provision and handed back when the sandbox ends. Both are
// best-effort: a cache that cannot be had costs time and nothing else, so every failure
// here is a line in the log and never an answer to the master.
pub(super) struct CacheLease {
    pub(super) project: String,
    // The moment the sandbox was ready, restored files and the sandbox's own managed
    // files included: whatever is newer than this was written by the task.
    pub(super) ready_at: SystemTime,
}

impl WorkerService {
    pub(super) async fn restore_cache(&self, project: &str) {
        let directory = self.config.project_cache_dir(project);
        let Some(archive) = self.layers.fetch_archive(ArchiveKind::Cache, project).await else {
            return;
        };
        let began = Instant::now();
        let unpacked = {
            let (archive, directory) = (archive.clone(), directory.clone());
            spawn_blocking(move || -> Result<(), String> {
                fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
                let file = fs::File::open(&archive).map_err(|error| error.to_string())?;
                Archive::new(file)
                    .unpack(&directory)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result)
        };
        let _ = fs::remove_file(&archive);
        match unpacked {
            Ok(()) => tracing::info!(
                project,
                elapsed_ms = began.elapsed().as_millis() as u64,
                "restored this project's cache from the master"
            ),
            // Half a cache is worse than none: a file cut short by a full disk would be
            // trusted by the next build.
            Err(error) => {
                tracing::warn!(%error, project, "a stored project cache would not restore");
                let _ = fs::remove_dir_all(&directory);
            }
        }
    }

    // Started once the sandbox is gone, because nothing is writing the cache after that.
    // The upload outlives this call, which is the point: the sandbox ending waits on
    // nothing. `settle_cache` is how the master lets the host live until it is done.
    pub(super) fn begin_cache_upload(&self, sandbox: &str) {
        let Some(lease) = self
            .caches
            .lock()
            .ok()
            .and_then(|mut caches| caches.remove(sandbox))
        else {
            return;
        };
        let service = self.clone();
        let upload = tokio::spawn(async move { service.upload_cache(lease).await });
        if let Ok(mut uploads) = self.uploads.lock() {
            uploads.insert(sandbox.to_owned(), upload);
        }
    }

    pub(super) async fn settle_cache(&self, sandbox: &str) {
        let upload = self
            .uploads
            .lock()
            .ok()
            .and_then(|mut uploads| uploads.remove(sandbox));
        if let Some(upload) = upload {
            let _ = upload.await;
        }
    }

    async fn upload_cache(&self, lease: CacheLease) {
        let directory = self.config.project_cache_dir(&lease.project);
        let ready_at = lease.ready_at;
        let packed = spawn_blocking(move || pack_if_changed(&directory, ready_at)).await;
        match packed {
            Ok(Ok(Some(archive))) => {
                self.layers
                    .send_archive(ArchiveKind::Cache, &lease.project, archive.path())
                    .await;
            }
            Ok(Ok(None)) => tracing::info!(
                project = %lease.project,
                "this project's cache is as the master has it; nothing to leave"
            ),
            Ok(Err(error)) => {
                tracing::warn!(%error, project = %lease.project, "could not pack this project's cache")
            }
            Err(error) => {
                tracing::warn!(%error, project = %lease.project, "packing this project's cache stopped")
            }
        }
    }
}

// An unchanged cache is not packed at all, which is the only saving available without a
// protocol that exchanges differences: the next host would restore the same bytes, and
// the upload is the slow part. Anything the task wrote, deleted or created moves a
// modification time past the moment the sandbox was ready.
fn pack_if_changed(
    directory: &Path,
    ready_at: SystemTime,
) -> Result<Option<NamedTempFile>, String> {
    if !changed_since(directory, ready_at) {
        return Ok(None);
    }
    let archive = NamedTempFile::new().map_err(|error| error.to_string())?;
    let file = archive.reopen().map_err(|error| error.to_string())?;
    let mut builder = Builder::new(file);
    // A link is stored as a link: following it would copy what it points at, which may
    // be outside the cache, and would turn a store of links into a store of copies.
    builder.follow_symlinks(false);
    // An entry that cannot be packed (a socket, a file nobody may read) is left out and
    // said aloud: one of them is not a reason for every host after this one to start cold.
    let mut pending = vec![(directory.to_path_buf(), String::from("."))];
    while let Some((path, name)) = pending.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            tracing::warn!(path = %path.display(), "leaving a directory out of a cache");
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            let child_name = format!("{name}/{}", entry.file_name().to_string_lossy());
            let Ok(metadata) = fs::symlink_metadata(&child) else {
                continue;
            };
            let packed = if metadata.is_dir() {
                pending.push((child.clone(), child_name.clone()));
                builder.append_dir(&child_name, &child)
            } else if metadata.is_file() || metadata.is_symlink() {
                builder.append_path_with_name(&child, &child_name)
            } else {
                continue;
            };
            if let Err(error) = packed {
                tracing::warn!(path = %child.display(), %error, "leaving an entry out of a cache");
            }
        }
    }
    builder.finish().map_err(|error| error.to_string())?;
    Ok(Some(archive))
}

fn changed_since(directory: &Path, ready_at: SystemTime) -> bool {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if metadata.modified().is_ok_and(|written| written > ready_at) {
                return true;
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    false
}
