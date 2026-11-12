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
    io::{AsyncReadExt, copy},
};

use crate::config::Config;
use crate::remote::protocol::{read_frame, write_frame};

// A host dials this to hand over the installs layer it built, or to ask for one that
// another host built. It is neither the link (clients and their tasks) nor the worker
// ALPN (the master asking a host for compute), for the same reason the cache has one
// of its own: it is a different conversation.
pub(crate) const LAYER_ALPN: &[u8] = b"altitude/layers/1";

// Long enough for a toolchain the agent's own profile installs, and no longer: this
// bounds what a host can put on its master's disk, not what a layer realistically is.
const MAX_LAYER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

// The window a stored layer is trusted for, which is the one base snapshots refresh
// on. Past it a host builds again rather than restore software nobody has looked at
// since.
const KEEP_FOR: Duration = Duration::from_secs(72 * 60 * 60);

// The most layers kept at once, oldest dropped first. An image has one per agent
// profile; this is room for a few generations of that.
const KEEP_COUNT: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LayerRequest {
    pub token: String,
    // What the host is: the image's own name, as the host declares it. A layer built
    // on one image is not the software another image's hosts expect to find.
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

// What a master keeps of the layers its hosts built. Nothing here knows what a layer
// is: an archive per environment and tag, aged out like a base snapshot. A host is
// trusted to say which environment it is, because the token is the boundary on this
// link and everywhere else: a host that lies about itself only corrupts its own
// fleet's layers.
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

    // Both halves are hashed rather than trusted: an environment is an image's name
    // and a tag is a digest, and neither may become a path.
    fn archive_path(&self, environment: &str, tag: &str) -> PathBuf {
        let key = blake3::hash(format!("{environment}\u{0}{tag}").as_bytes()).to_hex();
        self.dir.join(format!("{}.tar", &key[..32]))
    }

    fn fresh_archive(&self, environment: &str, tag: &str) -> Option<(PathBuf, u64)> {
        let path = self.archive_path(environment, tag);
        let metadata = fs::metadata(&path).ok()?;
        let age = metadata
            .modified()
            .ok()
            .and_then(|written| SystemTime::now().duration_since(written).ok());
        if age.is_some_and(|age| age >= KEEP_FOR) {
            // A layer this old would be rebuilt by its host anyway, so it is dropped
            // rather than left to fill the disk.
            let _ = fs::remove_file(&path);
            return None;
        }
        Some((path, metadata.len()))
    }

    fn adopt(&self, environment: &str, tag: &str, received: PathBuf) -> io::Result<u64> {
        let path = self.archive_path(environment, tag);
        fs::rename(&received, &path)?;
        let len = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        self.prune();
        Ok(len)
    }

    // Oldest out first, once there are more than KEEP_COUNT. A receive that never
    // finished leaves an `.incoming` file, which is nobody's layer and goes once it is
    // old enough to be one that never will.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let abandoned = SystemTime::now()
            .checked_sub(Duration::from_secs(60 * 60))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut archives: Vec<(SystemTime, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(written) = entry.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("tar") => archives.push((written, path)),
                Some("incoming") if written < abandoned => {
                    let _ = fs::remove_file(path);
                }
                _ => {}
            }
        }
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
        match request.op {
            LayerOp::Fetch => match self.store.fresh_archive(&request.environment, &request.tag) {
                Some((path, len)) => {
                    let Ok(mut archive) = AsyncFile::open(&path).await else {
                        let _ = write_frame(
                            &mut send,
                            &LayerReply::Failed("a stored layer could not be read".into()),
                        )
                        .await;
                        return Ok(());
                    };
                    if write_frame(&mut send, &LayerReply::Blob { len })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                    if let Err(error) = copy(&mut archive, &mut send).await {
                        tracing::warn!(%error, "a layer archive did not reach its host");
                        return Ok(());
                    }
                    let _ = send.finish();
                    return Ok(());
                }
                None => {
                    let _ = write_frame(&mut send, &LayerReply::Missing).await;
                }
            },
            LayerOp::Store { len } => {
                let reply = if len == 0 || len > MAX_LAYER_BYTES {
                    LayerReply::Failed(format!("{len} bytes is not a layer"))
                } else {
                    let destination = self.store.archive_path(&request.environment, &request.tag);
                    match receive(&mut recv, &destination, len).await {
                        Ok(received) => {
                            match self
                                .store
                                .adopt(&request.environment, &request.tag, received)
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
                };
                let _ = write_frame(&mut send, &reply).await;
            }
        }
        let _ = send.finish();
        Ok(())
    }
}

// The archive as it arrives. Kept beside where it will live, so finishing it is a
// rename on one filesystem: a half-received layer would be one a host restores and
// cannot run.
async fn receive(recv: &mut RecvStream, destination: &Path, len: u64) -> Result<PathBuf, String> {
    let incoming = destination.with_extension("incoming");
    let mut file = AsyncFile::create(&incoming)
        .await
        .map_err(|error| error.to_string())?;
    let copied = copy(&mut recv.take(len), &mut file)
        .await
        .map_err(|error| error.to_string())?;
    if copied != len {
        let _ = fs::remove_file(&incoming);
        return Err(format!("{copied} of {len} bytes of a layer arrived"));
    }
    file.sync_all().await.map_err(|error| error.to_string())?;
    Ok(incoming)
}
