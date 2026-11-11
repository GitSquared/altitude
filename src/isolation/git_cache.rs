use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const BUDGET_MIB: u32 = 10 * 1_024;

const BUDGET_BYTES: u64 = BUDGET_MIB as u64 * 1024 * 1024;

// What the guest may add to the mirrors, which is what this cache is willing to hold:
// a sandbox can fill the cache and hand the next one a warm mirror, but not more than
// eviction would keep anyway.
pub(crate) const GUEST_QUOTA_MIB: u32 = BUDGET_MIB;

const QUIET_FOR: Duration = Duration::from_secs(60 * 60);

pub(crate) const GUEST_PATH: &str = "/altitude/git-cache";

const MIRROR_SUFFIX: &str = ".git";

pub(crate) const ENVIRONMENT: &str = "ALTITUDE_GIT_CACHE";

pub fn ready(cache_dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(cache_dir)?;
    evict(cache_dir);
    // Resolved, because a directory reached through a symlink cannot be opened as
    // a mount, and a cache directory is exactly the sort of place that ends up
    // under one. The profile mounts are resolved the same way, for the same
    // reason.
    fs::canonicalize(cache_dir)
}

fn evict(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    let mut mirrors = Vec::new();
    let mut total = 0;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        if !name.ends_with(MIRROR_SUFFIX) || !path.is_dir() {
            continue;
        }
        let (bytes, last_used) = measure(&path);
        total += bytes;
        mirrors.push(Mirror {
            path,
            bytes,
            last_used,
        });
    }
    if total <= BUDGET_BYTES {
        return;
    }

    mirrors.sort_by_key(|mirror| mirror.last_used);
    let quiet_since = SystemTime::now()
        .checked_sub(QUIET_FOR)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    for mirror in mirrors {
        if total <= BUDGET_BYTES {
            break;
        }
        if mirror.last_used > quiet_since {
            continue;
        }
        // The lock file is a sibling rather than inside the mirror, so it
        // outlives nothing and is dropped with the mirror it guards.
        let _ = fs::remove_file(lock_path(&mirror.path));
        match fs::remove_dir_all(&mirror.path) {
            Ok(()) => {
                total = total.saturating_sub(mirror.bytes);
                tracing::info!(mirror = %mirror.path.display(), "dropped a cached repository");
            }
            Err(error) => {
                tracing::warn!(%error, mirror = %mirror.path.display(), "could not drop a cached repository")
            }
        }
    }
}

struct Mirror {
    path: PathBuf,
    bytes: u64,
    last_used: SystemTime,
}

fn measure(path: &Path) -> (u64, SystemTime) {
    let mut bytes = 0;
    let mut last_used = modified(fs::metadata(path).ok());
    let Ok(entries) = fs::read_dir(path) else {
        return (bytes, last_used);
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            let (inner_bytes, inner_last_used) = measure(&entry.path());
            bytes += inner_bytes;
            last_used = last_used.max(inner_last_used);
        } else {
            bytes += metadata.len();
            last_used = last_used.max(modified(Some(metadata)));
        }
    }
    (bytes, last_used)
}

fn modified(metadata: Option<fs::Metadata>) -> SystemTime {
    metadata
        .and_then(|metadata| metadata.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn lock_path(mirror: &Path) -> PathBuf {
    let mut name = mirror.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    mirror.with_file_name(name)
}
