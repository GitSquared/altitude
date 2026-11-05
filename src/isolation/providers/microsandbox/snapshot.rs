use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use microsandbox::snapshot::Snapshot;

use crate::isolation::{BaseSandboxRecipe, BaseSandboxSnapshot, IsolationError};

pub(super) struct BaseSandboxSnapshotStore {
    snapshots_dir: PathBuf,
    refresh_after: Duration,
}

pub(super) fn build_namespace(snapshots_dir: &Path) -> String {
    blake3::hash(snapshots_dir.to_string_lossy().as_bytes()).to_hex()[..12].to_owned()
}

pub(super) fn artifact_name(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_dash = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_') {
            slug.push(character);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_start_matches(|character: char| !character.is_ascii_alphanumeric());
    // Long names are cut here rather than by the runtime: the prefix and suffix
    // this is put between are fixed, and the readable part is what can give.
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_end_matches(['-', '.']);
    if slug.is_empty() {
        "project".to_owned()
    } else {
        slug.to_owned()
    }
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis()
        .try_into()
        .expect("millisecond timestamp does not fit in i64")
}

impl BaseSandboxSnapshotStore {
    pub(super) fn new(recipe: &BaseSandboxRecipe) -> Self {
        Self::at(recipe.snapshots_dir.clone(), recipe.refresh_after)
    }

    pub(super) fn at(snapshots_dir: PathBuf, refresh_after: Duration) -> Self {
        Self {
            snapshots_dir,
            refresh_after,
        }
    }

    pub(super) fn current(&self) -> Result<Option<BaseSandboxSnapshot>, IsolationError> {
        let manifest = match fs::read_to_string(self.snapshots_dir.join("current")) {
            Ok(manifest) => manifest,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(IsolationError::new(error.to_string())),
        };
        // Every field but the last is single-line by nature, and the setup
        // command is last because it is not.
        let mut fields = manifest.splitn(5, '\n');
        let Some(relative_path) = fields.next().filter(|path| !path.is_empty()) else {
            return Err(IsolationError::new(
                "base snapshot manifest has no snapshot path",
            ));
        };
        let Some(created_at_ms) = fields.next() else {
            return Err(IsolationError::new(
                "base snapshot manifest has no creation time",
            ));
        };
        let created_at_ms = created_at_ms.parse().map_err(|_| {
            IsolationError::new("base snapshot manifest has an invalid creation time")
        })?;
        let path = self.snapshots_dir.join(relative_path);
        if !path.exists() {
            return Err(IsolationError::new(format!(
                "base snapshot is missing: {}",
                path.display()
            )));
        }
        Ok(Some(BaseSandboxSnapshot {
            path,
            created_at_ms,
            os_release: fields.next().unwrap_or_default().to_owned(),
            packages: fields.next().unwrap_or_default().to_owned(),
            setup_command: fields.next().unwrap_or_default().to_owned(),
        }))
    }

    pub(super) fn needs_refresh(
        &self,
        current: Option<&BaseSandboxSnapshot>,
        setup_command: &str,
    ) -> bool {
        let Some(current) = current else {
            return true;
        };
        current.setup_command != setup_command
            || now_ms().saturating_sub(current.created_at_ms)
                >= self
                    .refresh_after
                    .as_millis()
                    .try_into()
                    .unwrap_or(i64::MAX)
    }

    pub(super) fn publish(&self, snapshot: &BaseSandboxSnapshot) -> Result<(), IsolationError> {
        fs::create_dir_all(&self.snapshots_dir)
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let relative_path = snapshot
            .path
            .strip_prefix(&self.snapshots_dir)
            .map_err(|_| IsolationError::new("base snapshot is outside its environment store"))?
            .to_string_lossy();
        let manifest_path = self.snapshots_dir.join("current");
        let temporary_path = self
            .snapshots_dir
            .join(format!("current-{}.tmp", process::id()));
        fs::write(
            &temporary_path,
            format!(
                "{relative_path}\n{}\n{}\n{}\n{}",
                snapshot.created_at_ms,
                snapshot.os_release,
                snapshot.packages,
                snapshot.setup_command
            ),
        )
        .map_err(|error| IsolationError::new(error.to_string()))?;
        fs::rename(temporary_path, manifest_path)
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    pub(super) async fn stale_snapshots(&self) -> Result<Vec<PathBuf>, IsolationError> {
        let current = self.current()?.map(|snapshot| snapshot.path);
        let snapshots = Snapshot::list_dir(&self.snapshots_dir)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let mut stale = Vec::new();
        for snapshot in snapshots {
            let path = snapshot
                .path()
                .map_err(|error| IsolationError::new(error.to_string()))?;
            if Some(path) != current.as_deref() {
                stale.push(path.to_owned());
            }
        }
        Ok(stale)
    }
}

// What a store holds is a layer, and a layer is a set of commands: change the commands
// and the store's name changes with them, so the old one is never composed again and
// never swept by anything that would know how to find it. A build this old would be
// redone on its next use regardless, so removing it gives the disk back and costs
// nothing but the rebuild it was going to pay for anyway.
pub(super) async fn forget_abandoned_stores(roots: &[PathBuf], older_than: Duration) {
    for root in roots {
        let Ok(stores) = fs::read_dir(root) else {
            continue;
        };
        for store in stores.flatten() {
            let path = store.path();
            if !store.file_type().is_ok_and(|kind| kind.is_dir())
                || last_used(&path)
                    .is_none_or(|used| used.elapsed().unwrap_or_default() < older_than)
            {
                continue;
            }
            tracing::debug!(store = %path.display(), "forgetting an environment nothing composes");
            for snapshot in Snapshot::list_dir(&path).await.unwrap_or_default() {
                let Ok(snapshot) = snapshot.path() else {
                    continue;
                };
                if let Err(error) = Snapshot::remove(&snapshot.to_string_lossy(), true).await {
                    tracing::warn!(snapshot = %snapshot.display(), %error, "could not remove an abandoned environment snapshot");
                }
            }
            // Whatever the runtime left behind — the manifest, an empty directory — so
            // that the next walk does not find this store again.
            if let Err(error) = fs::remove_dir_all(&path) {
                tracing::warn!(store = %path.display(), %error, "could not remove an abandoned environment store");
            }
        }
    }
}

// Every build that publishes writes `current`, so that is when the store was last
// built; a store that never finished one falls back to its own time.
fn last_used(store: &Path) -> Option<SystemTime> {
    fs::metadata(store.join("current"))
        .and_then(|metadata| metadata.modified())
        .or_else(|_| fs::metadata(store).and_then(|metadata| metadata.modified()))
        .ok()
}
