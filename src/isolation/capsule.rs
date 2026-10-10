//! Preserve and verify the environment boundary used to export task layers.

use microsandbox::{
    Snapshot, SnapshotState,
    snapshot::{DiskLayer, Manifest, SnapshotFormat, SnapshotId},
};
use microsandbox_image::checkpoint::{relocate_qcow2_backing, sparse_file_integrity};
use microsandbox_utils::copy::fast_copy;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tokio::task::spawn_blocking;

pub(crate) const ENVIRONMENT_LABEL: &str = "altitude.environment";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct EnvironmentBase {
    pub manifest: Manifest,
    pub roots: Vec<String>,
}

impl EnvironmentBase {
    pub async fn from_environment(path: &str) -> Result<Self, String> {
        let snapshot = Snapshot::open(path).await.map_err(|e| e.to_string())?;
        let manifest = snapshot.manifest().clone();
        let paths = layers(&manifest)?
            .iter()
            .map(|layer| snapshot.layer_path(layer).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let roots = spawn_blocking(move || {
            paths
                .into_iter()
                .map(|path| {
                    sparse_file_integrity(&path)
                        .map(|value| value.root)
                        .map_err(|e| e.to_string())
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(|e| e.to_string())??;
        Ok(Self { manifest, roots })
    }

    pub async fn from_capsule(path: &str) -> Result<Self, String> {
        let snapshot = Snapshot::open(path).await.map_err(|e| e.to_string())?;
        let base = snapshot
            .labels()
            .get(ENVIRONMENT_LABEL)
            .ok_or_else(|| "the capsule has no recorded environment boundary".to_owned())?;
        serde_json::from_str(base).map_err(|e| e.to_string())
    }

    /// Relocate copied backing references before comparing bytes: SDK capture gives
    /// stopped layers fresh IDs even when their immutable contents have not changed.
    pub async fn stage(
        &self,
        snapshot: &Snapshot,
        directory: &Path,
    ) -> Result<(Snapshot, Snapshot), String> {
        let base = self.clone();
        let snapshot = snapshot.clone();
        let directory = directory.to_owned();
        let (base_path, task_path) =
            spawn_blocking(move || base.stage_files(&snapshot, &directory))
                .await
                .map_err(|e| e.to_string())??;
        let base = Snapshot::open(base_path.to_string_lossy())
            .await
            .map_err(|e| e.to_string())?;
        let task = Snapshot::open(task_path.to_string_lossy())
            .await
            .map_err(|e| e.to_string())?;
        Ok((base, task))
    }

    fn stage_files(
        &self,
        snapshot: &Snapshot,
        directory: &Path,
    ) -> Result<(PathBuf, PathBuf), String> {
        let mut base_manifest = self.manifest.clone();
        let base_count = layers(&base_manifest)?.len();
        if base_count != self.roots.len() {
            return Err("the environment integrity record is incomplete".into());
        }
        let SnapshotState::File(base_state) = &mut base_manifest.state else {
            return Err("only disk capsules can travel".into());
        };
        for layer in &mut base_state.layers {
            layer.payload.integrity = None;
        }
        base_manifest.parent = None;
        let identity = blake3::hash(
            &serde_json::to_vec(&(
                base_manifest
                    .to_canonical_bytes()
                    .map_err(|e| e.to_string())?,
                &self.roots,
            ))
            .map_err(|e| e.to_string())?,
        )
        .to_hex()
        .to_string();
        base_manifest.snapshot_id =
            SnapshotId::new(format!("snap_{}", &identity[..32])).map_err(|e| e.to_string())?;
        let base_layers = layers(&base_manifest)?;
        let task_layers = layers(snapshot.manifest())?;
        if task_layers.len() <= base_layers.len() {
            return Err("the capsule has no verified environment boundary".into());
        }
        let base_path = directory.join("environment");
        let task_path = directory.join("task");
        fs::create_dir_all(base_path.join("layers")).map_err(|e| e.to_string())?;
        fs::create_dir_all(task_path.join("layers")).map_err(|e| e.to_string())?;
        let mut manifest = snapshot.manifest().clone();
        let SnapshotState::File(state) = &mut manifest.state else {
            return Err("only disk capsules can travel".into());
        };
        for (index, layer) in task_layers.iter().enumerate() {
            let target_layer = if index < base_layers.len() {
                &base_layers[index]
            } else {
                layer
            };
            let filename = format!(
                "{}.{}",
                target_layer.layer_id,
                extension(target_layer.format)
            );
            let destination = task_path.join("layers").join(&filename);
            fast_copy(
                &snapshot.layer_path(layer).map_err(|e| e.to_string())?,
                &destination,
            )
            .map_err(|e| e.to_string())?;
            if index > 0 {
                let previous = &state.layers[index - 1];
                relocate_qcow2_backing(
                    &destination,
                    &task_path.join("layers").join(format!(
                        "{}.{}",
                        previous.layer_id,
                        extension(previous.format)
                    )),
                )
                .map_err(|e| e.to_string())?;
            }
            if index < base_layers.len() {
                let arrived = sparse_file_integrity(&destination)
                    .map_err(|e| e.to_string())?
                    .root;
                if arrived != self.roots[index] {
                    return Err(format!(
                        "environment layer {index} changed; refusing an unsafe delta"
                    ));
                }
                state.layers[index] = target_layer.clone();
                fs::hard_link(&destination, base_path.join("layers").join(filename))
                    .map_err(|e| e.to_string())?;
            } else {
                state.layers[index].backing = Some(state.layers[index - 1].layer_id.clone());
                // Relocation changes the header of our private copy.
                state.layers[index].payload.integrity = None;
            }
        }
        manifest.parent = Some(base_manifest.snapshot_id.clone());
        write_manifest(&base_path, &base_manifest)?;
        write_manifest(&task_path, &manifest)?;
        let metadata = snapshot
            .path()
            .map_err(|e| e.to_string())?
            .join("metadata.json");
        if metadata.exists() {
            fs::copy(metadata, task_path.join("metadata.json")).map_err(|e| e.to_string())?;
        }
        Ok((base_path, task_path))
    }
}

fn layers(manifest: &Manifest) -> Result<&[DiskLayer], String> {
    match &manifest.state {
        SnapshotState::File(state) => Ok(&state.layers),
        _ => Err("only disk capsules can travel".into()),
    }
}

fn write_manifest(directory: &Path, manifest: &Manifest) -> Result<(), String> {
    fs::write(
        directory.join("snapshot.json"),
        manifest.to_canonical_bytes().map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn extension(format: SnapshotFormat) -> &'static str {
    match format {
        SnapshotFormat::Raw => "raw",
        SnapshotFormat::Qcow2 => "qcow2",
    }
}

pub(crate) async fn verify_environment(
    snapshot: &Snapshot,
    expected: &[String],
) -> Result<(), String> {
    let paths = layers(snapshot.manifest())?
        .iter()
        .map(|layer| snapshot.layer_path(layer).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    if paths.len() != expected.len() {
        return Err("the environment integrity record is incomplete".into());
    }
    let expected = expected.to_vec();
    spawn_blocking(move || {
        for (path, expected) in paths.into_iter().zip(expected) {
            let actual = sparse_file_integrity(&path)
                .map_err(|e| e.to_string())?
                .root;
            if actual != expected {
                return Err("environment content differs from the prepared base".into());
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}
