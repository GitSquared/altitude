use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use iroh::{
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::File as AsyncFile,
    io::{AsyncReadExt, copy},
};

use crate::config::Config;
use crate::remote::protocol::{read_frame, write_frame};

// A host dials this to hand over what it built, or to ask for what another host left:
// the installs layer, and a project's cache. It is neither the link (clients and their
// tasks) nor the worker ALPN (the master asking a host for compute): it is a different
// conversation.
pub(crate) const LAYER_ALPN: &[u8] = b"altitude/layers/1";

const GIB: u64 = 1024 * 1024 * 1024;

// What the master keeps for its hosts. Each kind has its own directory and its own
// budget, so a project's cache, which is large and rewritten by every task, cannot
// push out the layers every host of an image restores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ArchiveKind {
    // The software a host installed, shared by every project.
    Layer,
    // One project's cache directories as the last sandbox to end left them.
    Cache,
}

impl ArchiveKind {
    fn name(self) -> &'static str {
        match self {
            Self::Layer => "layer",
            Self::Cache => "cache",
        }
    }

    // Bounds what a host can put on its master's disk, not what an archive realistically
    // is: a layer is a toolchain, a cache is a package store and may be gigabytes.
    fn max_bytes(self) -> u64 {
        match self {
            Self::Layer => 4 * GIB,
            Self::Cache => 8 * GIB,
        }
    }

    // A freshness rule and not a sweep, evaluated when a host asks, never on a timer.
    // A layer's commands install the newest release they name, so an old archive is
    // older software and 72 hours is the window base snapshots refresh on. A cache is
    // rebuildable data that only costs time when stale, so it is kept for longer.
    fn keep_for(self) -> Duration {
        match self {
            Self::Layer => Duration::from_secs(72 * 60 * 60),
            Self::Cache => Duration::from_secs(14 * 24 * 60 * 60),
        }
    }

    // What the kind may occupy. Eviction is a bucket, not a count of files: what costs
    // is bytes.
    fn budget(self) -> u64 {
        match self {
            Self::Layer => 8 * GIB,
            Self::Cache => 32 * GIB,
        }
    }

    // A host waiting on an archive it may not get is a host not doing its own work, so
    // asking is bounded as a whole, dial and download included. A cache is larger, and
    // still only ever costs the cold start the host would have had without it.
    pub(crate) fn fetch_patience(self) -> Duration {
        match self {
            Self::Layer => Duration::from_secs(10),
            Self::Cache => Duration::from_secs(2 * 60),
        }
    }

    // An upload moves the whole archive, so it gets minutes where a question gets
    // seconds, and it is still not left to the connection's ten-minute idle window.
    pub(crate) fn store_patience(self) -> Duration {
        match self {
            Self::Layer => Duration::from_secs(5 * 60),
            Self::Cache => Duration::from_secs(10 * 60),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LayerRequest {
    pub token: String,
    pub kind: ArchiveKind,
    // What the host is: the image's own name, as the host declares it. A layer built
    // on one image is not the software another image's hosts expect to find.
    pub environment: String,
    // The archive's name for itself: the digest of the commands that build a layer, the
    // project's id for a cache.
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

// What a master keeps of what its hosts leave. Nothing here knows what an archive is:
// one per kind, environment and tag, aged out like a base snapshot. A host is trusted to
// say which environment it is, because the token is the boundary on this link and
// everywhere else: a host that lies about itself only corrupts its own fleet's archives.
pub(crate) struct LayerStore {
    dir: PathBuf,
    caches: PathBuf,
    // Where a receive writes before it is an archive. Emptied when the store opens: a
    // file here is a receive that did not finish, and nothing can be receiving while
    // this process is starting.
    staging: PathBuf,
    token: String,
}

impl LayerStore {
    pub(crate) fn open(config: &Config, token: String) -> io::Result<Self> {
        let dir = config.data_dir().join("hosts").join("layers");
        let caches = dir.join("caches");
        let staging = dir.join("staging");
        fs::create_dir_all(&staging)?;
        fs::create_dir_all(&caches)?;
        // A store nobody can write to would fail every upload one host at a time, with
        // a reason only the master's log can give, so it is said once, here.
        for directory in [&dir, &caches, &staging] {
            if let Err(error) = ensure_writable(directory) {
                tracing::error!(
                    path = %directory.display(),
                    %error,
                    "the archive store is not writable; hosts cannot leave their layers or caches here"
                );
            }
        }
        for leftover in fs::read_dir(&staging)?.flatten() {
            let _ = fs::remove_file(leftover.path());
        }
        Ok(Self {
            dir,
            caches,
            staging,
            token,
        })
    }

    // Unique per receive: two tasks of one project end at once, and a shared name would
    // have them write the same file and hand the master an archive of both.
    fn staging_path(&self, request: &LayerRequest) -> PathBuf {
        self.staging.join(format!(
            "{}.{}.incoming",
            self.key(request.kind, &request.environment, &request.tag),
            uuid::Uuid::new_v4()
        ))
    }

    fn key(&self, kind: ArchiveKind, environment: &str, tag: &str) -> String {
        let key = blake3::hash(format!("{}\u{0}{environment}\u{0}{tag}", kind.name()).as_bytes())
            .to_hex();
        key[..32].to_owned()
    }

    fn directory(&self, kind: ArchiveKind) -> &Path {
        match kind {
            ArchiveKind::Layer => &self.dir,
            ArchiveKind::Cache => &self.caches,
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    // Both halves are hashed rather than trusted: an environment is an image's name
    // and a tag is a digest, and neither may become a path.
    fn archive_path(&self, kind: ArchiveKind, environment: &str, tag: &str) -> PathBuf {
        self.directory(kind)
            .join(format!("{}.tar", self.key(kind, environment, tag)))
    }

    fn fresh_archive(
        &self,
        kind: ArchiveKind,
        environment: &str,
        tag: &str,
    ) -> Option<(PathBuf, u64)> {
        let path = self.archive_path(kind, environment, tag);
        let metadata = fs::metadata(&path).ok()?;
        let age = metadata
            .modified()
            .ok()
            .and_then(|written| SystemTime::now().duration_since(written).ok());
        if age.is_some_and(|age| age >= kind.keep_for()) {
            // An archive this old is rebuilt by its host anyway, so it is dropped rather
            // than left to fill the disk.
            let _ = fs::remove_file(&path);
            return None;
        }
        Some((path, metadata.len()))
    }

    fn adopt(&self, request: &LayerRequest, received: PathBuf) -> io::Result<u64> {
        let path = self.archive_path(request.kind, &request.environment, &request.tag);
        // The path matters as much as the reason: a store that refuses says which file
        // it refused, because the host that sent it can do nothing with "denied".
        fs::rename(&received, &path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{} -> {}: {error}", received.display(), path.display()),
            )
        })?;
        let len = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        self.enforce_budget(request.kind, &path);
        Ok(len)
    }

    // The one guard on what the store costs, checked where it can change and nowhere
    // else: after a write, while the store is over its budget, drop the archive nobody
    // has written since.
    fn enforce_budget(&self, kind: ArchiveKind, keep: &Path) {
        let Ok(entries) = fs::read_dir(self.directory(kind)) else {
            return;
        };
        let mut archives: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("tar") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let written = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total += metadata.len();
            archives.push((written, metadata.len(), path));
        }
        if total <= kind.budget() {
            return;
        }
        archives.sort();
        for (_, len, path) in archives {
            if total <= kind.budget() {
                break;
            }
            if path == keep {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

#[derive(Clone)]
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
        // A host asks for a layer and a cache per sandbox and leaves them on the way out,
        // all on the connection it holds, so each stream is answered as it arrives.
        loop {
            let Ok((send, recv)) = connection.accept_bi().await else {
                return Ok(());
            };
            let handler = self.clone();
            tokio::spawn(async move { handler.answer_layer(send, recv).await });
        }
    }
}

impl LayerHandler {
    async fn answer_layer(&self, mut send: SendStream, mut recv: RecvStream) {
        // A host waiting on an answer it never gets is indistinguishable from a master
        // that never heard the question, and only one of those is a bug here.
        let request: LayerRequest = match read_frame(&mut recv).await {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "an unreadable layer call arrived");
                return;
            }
        };
        tracing::info!(kind = request.kind.name(), tag = %request.tag, "a host called the archive store");
        if request.token != self.store.token() {
            let _ = write_frame(
                &mut send,
                &LayerReply::Failed("this host does not hold the master's token".into()),
            )
            .await;
            return;
        }
        match request.op {
            LayerOp::Fetch => {
                match self
                    .store
                    .fresh_archive(request.kind, &request.environment, &request.tag)
                {
                    Some((path, len)) => {
                        let Ok(mut archive) = AsyncFile::open(&path).await else {
                            let _ = write_frame(
                                &mut send,
                                &LayerReply::Failed("a stored archive could not be read".into()),
                            )
                            .await;
                            return;
                        };
                        if write_frame(&mut send, &LayerReply::Blob { len })
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if let Err(error) = copy(&mut archive, &mut send).await {
                            tracing::warn!(%error, "an archive did not reach its host");
                            return;
                        }
                        let _ = send.finish();
                        return;
                    }
                    None => {
                        let _ = write_frame(&mut send, &LayerReply::Missing).await;
                    }
                }
            }
            LayerOp::Store { len } => {
                let reply = if len == 0 || len > request.kind.max_bytes() {
                    LayerReply::Failed(format!("{len} bytes is not a {}", request.kind.name()))
                } else {
                    let staging = self.store.staging_path(&request);
                    match receive(&mut recv, &staging, len).await {
                        Ok(received) => match self.store.adopt(&request, received) {
                            Ok(len) => {
                                tracing::info!(
                                    kind = request.kind.name(),
                                    environment = %request.environment,
                                    tag = %request.tag,
                                    bytes = len,
                                    "a host left an archive for the next one"
                                );
                                LayerReply::Stored
                            }
                            Err(error) => LayerReply::Failed(error.to_string()),
                        },
                        Err(error) => LayerReply::Failed(error),
                    }
                };
                let _ = write_frame(&mut send, &reply).await;
            }
        }
        let _ = send.finish();
        // Only now: stopping the archive's stream before the reply is out is what hid
        // the reason from a host whose upload this refused.
        let _ = recv.stop(0u32.into());
    }
}

fn ensure_writable(directory: &Path) -> io::Result<()> {
    let probe = directory.join(".writable-probe");
    fs::write(&probe, b"")?;
    fs::remove_file(&probe)
}

// The archive as it arrives. Kept beside where it will live, so finishing it is a
// rename on one filesystem: a half-received archive would be one a host restores and
// cannot unpack.
async fn receive(recv: &mut RecvStream, destination: &Path, len: u64) -> Result<PathBuf, String> {
    let incoming = destination.with_extension("incoming");
    let mut file = AsyncFile::create(&incoming)
        .await
        .map_err(|error| format!("{}: {error}", incoming.display()))?;
    let received = async {
        let copied = copy(&mut recv.take(len), &mut file)
            .await
            .map_err(|error| error.to_string())?;
        if copied != len {
            return Err(format!("{copied} of {len} bytes of an archive arrived"));
        }
        file.sync_all().await.map_err(|error| error.to_string())
    }
    .await;
    if let Err(error) = received {
        let _ = fs::remove_file(&incoming);
        return Err(error);
    }
    Ok(incoming)
}
