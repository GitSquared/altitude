use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use iroh::{
    endpoint::{Connection, RecvStream},
    protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::File as AsyncFile,
    io::{AsyncReadExt, copy as copy_stream},
};

use crate::config::Config;
use crate::remote::protocol::{MAX_BLOB_BYTES, read_frame, write_frame};

// A host dials this to hand over the machine layer it built, or to ask for one that
// another host built. It is neither the link (clients and their tasks) nor the worker
// ALPN (the master asking a host for compute): it is a host and its master trading
// the software a task's sandbox expects to find already installed.
pub(crate) const LAYER_ALPN: &[u8] = b"altitude/layers/1";

// How long a stored layer is worth trusting, which is the window altitude refreshes
// its own base snapshots on: past it a host builds again rather than restore software
// nobody has looked at since.
const KEEP_FOR: Duration = Duration::from_secs(72 * 60 * 60);

// The most layers kept at once, oldest dropped first. A remote at one image has one
// per agent profile; this is room for a few generations of that.
const KEEP_COUNT: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LayerRequest {
    pub token: String,
    // What the host is: the image's own name, as the host declares it. A layer built
    // on one image is not the software another image's boxes expect to find.
    pub environment: String,
    // The layer's name for itself, which is the digest of the commands that build it.
    pub tag: String,
    pub op: LayerOp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum LayerOp {
    Fetch,
    // The archive follows the request, `len` bytes of it.
    Store { len: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum LayerReply {
    // The archive follows, `len` bytes of it.
    Blob { len: u64 },
    Missing,
    Stored,
    Failed(String),
}

// What a master keeps of what its hosts built. Nothing here knows what a layer is:
// it is an archive per (environment, tag), aged out like the base snapshots it stands
// in for.
pub(crate) struct LayerStore {
    dir: PathBuf,
    token: String,
}

impl LayerStore {
    pub(crate) fn open(config: &Config, token: String) -> io::Result<Self> {
        let dir = config.data_dir().join("hosts").join("layers");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir, token })
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    // Named by a digest of both halves rather than by the halves themselves: an
    // environment is an image's name and a tag is a digest, and neither may be a path.
    fn path(&self, environment: &str, tag: &str) -> PathBuf {
        let key = blake3::hash(format!("{environment}\u{0}{tag}").as_bytes()).to_hex();
        self.dir.join(format!("{}.tar", &key[..32]))
    }

    // The archive, if there is one and it is young enough to trust.
    fn stored(&self, environment: &str, tag: &str) -> Option<(PathBuf, u64)> {
        let path = self.path(environment, tag);
        let metadata = fs::metadata(&path).ok()?;
        let fresh = metadata
            .modified()
            .ok()
            .and_then(|written| SystemTime::now().duration_since(written).ok())
            .is_none_or(|age| age < KEEP_FOR);
        if !fresh {
            // Aged out: drop it rather than leave the disk to fill with software
            // nobody would restore.
            let _ = fs::remove_file(&path);
            return None;
        }
        Some((path, metadata.len()))
    }

    fn keep(&self, environment: &str, tag: &str, incoming: PathBuf) -> io::Result<u64> {
        let path = self.path(environment, tag);
        fs::rename(&incoming, &path)?;
        let len = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        self.prune();
        Ok(len)
    }

    // Oldest out first, once there are more than KEEP_COUNT.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut archives: Vec<(SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "tar"))
            .filter_map(|entry| {
                let written = entry.metadata().ok()?.modified().ok()?;
                Some((written, entry.path()))
            })
            .collect();
        if archives.len() <= KEEP_COUNT {
            return;
        }
        archives.sort();
        let excess = archives.len() - KEEP_COUNT;
        for (_, path) in archives.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) struct LayerHandler {
    store: Arc<LayerStore>,
}

impl LayerHandler {
    pub(crate) fn new(store: Arc<LayerStore>) -> Self {
        Self { store }
    }
}

impl fmt::Debug for LayerHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LayerHandler")
    }
}

impl ProtocolHandler for LayerHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let Ok((mut send, mut recv)) = connection.accept_bi().await else {
            return Ok(());
        };
        let request: LayerRequest = match read_frame(&mut recv).await {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "an unreadable layer call arrived");
                return Ok(());
            }
        };
        if request.token != self.store.token() {
            let _ = write_frame(
                &mut send,
                &LayerReply::Failed("this host does not hold the master's token".into()),
            )
            .await;
            return Ok(());
        }
        let destination = self.store.path(&request.environment, &request.tag);
        let reply = match request.op {
            LayerOp::Fetch => match self.store.stored(&request.environment, &request.tag) {
                Some((path, len)) => match AsyncFile::open(&path).await {
                    Ok(mut archive) => {
                        if write_frame(&mut send, &LayerReply::Blob { len })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                        if let Err(error) = copy_stream(&mut archive, &mut send).await {
                            tracing::warn!(%error, "a layer archive did not reach its host");
                            return Ok(());
                        }
                        let _ = send.finish();
                        return Ok(());
                    }
                    Err(error) => LayerReply::Failed(error.to_string()),
                },
                None => LayerReply::Missing,
            },
            // The bytes arrive after the request, so they are kept beside the store and
            // renamed into it only once they are all there: a half-received archive
            // would be a layer a host restores and cannot run.
            LayerOp::Store { len } => {
                if len > MAX_BLOB_BYTES {
                    LayerReply::Failed(format!("{len} bytes is more than a layer may be"))
                } else {
                    match receive(&mut recv, &destination, len).await {
                        Ok((incoming, identical)) => {
                            let _ = identical;
                            match self
                                .store
                                .keep(&request.environment, &request.tag, incoming)
                            {
                                Ok(len) => {
                                    tracing::info!(
                                        environment = %request.environment,
                                        tag = %request.tag,
                                        bytes = len,
                                        "a host left a built layer for the next one"
                                    );
                                    LayerReply::Stored
                                }
                                Err(error) => LayerReply::Failed(error.to_string()),
                            }
                        }
                        Err(error) => LayerReply::Failed(error),
                    }
                }
            }
        };
        let _ = write_frame(&mut send, &reply).await;
        let _ = send.finish();
        Ok(())
    }
}

// The archive as it arrives, into a file beside where it will live so the rename that
// finishes it is on one filesystem.
async fn receive(
    recv: &mut RecvStream,
    destination: &Path,
    len: u64,
) -> Result<(PathBuf, bool), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| "the layer store has no directory".to_owned())?;
    let incoming = destination.with_extension("incoming");
    let mut file = AsyncFile::create(&incoming)
        .await
        .map_err(|error| error.to_string())?;
    let copied = copy_stream(&mut recv.take(len), &mut file)
        .await
        .map_err(|error| error.to_string())?;
    if copied != len {
        let _ = fs::remove_file(&incoming);
        return Err(format!("{copied} of {len} bytes of a layer arrived"));
    }
    file.sync_all().await.map_err(|error| error.to_string())?;
    let _ = parent;
    Ok((incoming, false))
}
