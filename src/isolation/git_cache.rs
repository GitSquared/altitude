//! The shared git object cache on this machine.
//!
//! A capsule is disposable, so the repository cloned into it is too, and a
//! machine running task after task over the same few repositories was pulling
//! each one from the network again every time. The cache is one bare mirror per
//! repository, mounted into every capsule, and the git a capsule finds in its
//! `PATH` clones through it: it borrows the objects the mirror already holds and
//! copies what it borrowed into the result.
//!
//! That last part is why the cache is disposable. A clone that copied what it
//! borrowed is the repository's own, so this directory can be deleted — by the
//! sweep below, by the operator, by an operating system reclaiming its cache
//! folder — and the only thing lost is the network traffic it would have saved.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// How much the cache may hold before it starts dropping the repositories it has
/// gone longest without using.
///
/// Deliberately generous, because what the cache buys back is network traffic
/// rather than disk: a capsule is disposable and its disk writes are cheap, so a
/// mirror dropped to save space is a repository pulled again next time.
const BUDGET_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// How long a mirror is left alone after it was last used.
///
/// A task clones *through* a mirror, borrowing its objects until it has copied
/// them in, so a mirror dropped in the middle of that fails the clone rather
/// than slowing it down. A task marks the mirror as it starts, so an hour is
/// longer than any clone and shorter than this machine's interest in a
/// repository.
///
/// A sweep that cannot get under its budget without breaking this leaves the
/// cache over budget, and takes it up again the next time a capsule is created.
const QUIET_FOR: Duration = Duration::from_secs(60 * 60);

/// Where a task finds the cache.
pub(crate) const GUEST_PATH: &str = "/altitude/git-cache";

/// The mirror file name suffix, which is what marks a directory here as one.
///
/// A bare repository's directory is named after the repository, so the suffix
/// keeps the sweep from touching anything else that ends up in the cache.
const MIRROR_SUFFIX: &str = ".git";

/// The variable a capsule's shell reads [`GUEST_PATH`] from, which is how the git
/// a profile installs finds the cache.
pub(crate) const ENVIRONMENT: &str = "ALTITUDE_GIT_CACHE";

/// Make this machine's git cache ready for a capsule, and say where it is on this
/// machine.
///
/// One step rather than three, because a capsule is given the cache exactly once
/// — when it is created — and the sweep is what keeps the directory from growing
/// without limit.
pub fn ready(cache_dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(cache_dir)?;
    evict(cache_dir);
    // Resolved, because a directory reached through a symlink cannot be opened as
    // a mount, and a cache directory is exactly the sort of place that ends up
    // under one. The profile mounts are resolved the same way, for the same
    // reason.
    fs::canonicalize(cache_dir)
}

/// Drop whole mirrors, least recently used first, until the cache is back under
/// its budget.
///
/// Safe at any moment, because a repository that has finished cloning holds the
/// objects it borrowed and the one that has not is inside [`QUIET_FOR`].
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

/// What a directory holds, and when anything in it was last written.
///
/// A mirror is touched by every task that clones through it, and the touch is
/// what makes least-recently-used mean used rather than fetched.
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

/// The file a capsule locks a mirror with while it updates it.
///
/// Named here rather than by the shell that writes it, because two things have
/// to agree on it: the updater, and the sweep that drops the mirror.
fn lock_path(mirror: &Path) -> PathBuf {
    let mut name = mirror.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    mirror.with_file_name(name)
}
