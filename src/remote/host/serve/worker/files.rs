use std::{fs, path::Path};

use tempfile::{NamedTempFile, TempDir};

use iroh::endpoint::{RecvStream, SendStream};
use tokio::{fs::File as AsyncFile, io::copy};

use crate::{
    isolation::{IsolationEngine, UnwatchedFiles, worker_protocol::WorkerReply},
    remote::protocol::write_frame,
};

use super::WorkerService;

impl WorkerService {
    // The bytes on the stream are a file the master is putting in the sandbox.
    pub(super) async fn receive_file(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
        id: String,
        guest_path: String,
    ) {
        let reply = match receive_to_temp(&mut recv).await {
            Ok(temp) => {
                match self
                    .engine
                    .deliver_file(id, temp.path().to_path_buf(), guest_path)
                    .await
                {
                    Ok(()) => WorkerReply::Ok,
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            Err(error) => WorkerReply::Failed(error),
        };
        let _ = write_frame(&mut send, &reply).await;
        let _ = send.finish();
    }

    // The bytes on the stream are a tar of a tree the master is putting in.
    pub(super) async fn receive_tree(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
        id: String,
        root: String,
    ) {
        let reply = match receive_to_temp(&mut recv).await {
            Ok(tar) => match TempDir::new() {
                Ok(directory) => match extract_tree(tar.path(), directory.path()) {
                    Ok(()) => match self
                        .engine
                        .deliver_files(id, directory.path().to_path_buf(), root, &UnwatchedFiles)
                        .await
                    {
                        Ok(_) => WorkerReply::Ok,
                        Err(error) => WorkerReply::Failed(error.to_string()),
                    },
                    Err(error) => WorkerReply::Failed(error),
                },
                Err(error) => WorkerReply::Failed(error.to_string()),
            },
            Err(error) => WorkerReply::Failed(error),
        };
        let _ = write_frame(&mut send, &reply).await;
        let _ = send.finish();
    }

    pub(super) async fn send_file(
        &self,
        mut send: SendStream,
        _recv: RecvStream,
        id: String,
        guest_path: String,
    ) {
        if let Ok(temp) = NamedTempFile::new()
            && let Ok(len) = self
                .engine
                .collect_file(id, guest_path, temp.path().to_path_buf())
                .await
        {
            if write_frame(
                &mut send,
                &WorkerReply::Blob {
                    len,
                    files: 1,
                    bytes: len,
                },
            )
            .await
            .is_ok()
                && let Ok(mut file) = AsyncFile::open(temp.path()).await
            {
                let _ = copy(&mut file, &mut send).await;
            }
            let _ = send.finish();
            return;
        }
        let _ = write_frame(
            &mut send,
            &WorkerReply::Failed("could not collect that file".into()),
        )
        .await;
        let _ = send.finish();
    }

    pub(super) async fn send_tree(
        &self,
        mut send: SendStream,
        _recv: RecvStream,
        id: String,
        root: String,
        excluded: Vec<String>,
    ) {
        if let Ok(directory) = TempDir::new()
            && let Ok(tally) = self
                .engine
                .collect_files(
                    id,
                    root,
                    excluded,
                    directory.path().to_path_buf(),
                    &UnwatchedFiles,
                )
                .await
            && let Ok(tar) = NamedTempFile::new()
            && let Ok(len) = pack_tree(directory.path(), tar.path())
        {
            if write_frame(
                &mut send,
                &WorkerReply::Blob {
                    len,
                    files: tally.files,
                    bytes: tally.bytes,
                },
            )
            .await
            .is_ok()
                && let Ok(mut file) = AsyncFile::open(tar.path()).await
            {
                let _ = copy(&mut file, &mut send).await;
            }
            let _ = send.finish();
            return;
        }
        let _ = write_frame(
            &mut send,
            &WorkerReply::Failed("could not collect that tree".into()),
        )
        .await;
        let _ = send.finish();
    }
}

// The bytes on an incoming stream, kept in a temporary file the engine can read as
// a host path.
async fn receive_to_temp(recv: &mut RecvStream) -> Result<NamedTempFile, String> {
    let temp = NamedTempFile::new().map_err(|error| error.to_string())?;
    let mut file = AsyncFile::create(temp.path())
        .await
        .map_err(|error| error.to_string())?;
    copy(recv, &mut file)
        .await
        .map_err(|error| error.to_string())?;
    file.sync_all().await.map_err(|error| error.to_string())?;
    Ok(temp)
}

fn pack_tree(source: &Path, tar_path: &Path) -> Result<u64, String> {
    let file = fs::File::create(tar_path).map_err(|error| error.to_string())?;
    let mut builder = tar::Builder::new(file);
    builder
        .append_dir_all(".", source)
        .map_err(|error| error.to_string())?;
    builder.finish().map_err(|error| error.to_string())?;
    fs::metadata(tar_path)
        .map(|metadata| metadata.len())
        .map_err(|error| error.to_string())
}

fn extract_tree(tar_path: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let file = fs::File::open(tar_path).map_err(|error| error.to_string())?;
    let mut archive = tar::Archive::new(file);
    archive
        .unpack(destination)
        .map_err(|error| error.to_string())
}
