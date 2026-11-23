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
pub(crate) const LAYER_ALPN: &[u8] = b"altitude/layers/2";

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

// A layer installs the newest release its commands name, so one older than this is
// older software, and 72 hours is the window base snapshots refresh on. A cache has no
// such rule: a stale package store is still correct, and the byte budget is what bounds it.
const LAYER_FRESHNESS: Duration = Duration::from_secs(72 * 60 * 60);

// The one assumption behind every cache bound: a host moves a cache to or from the
// master at about this rate (200 Mbit/s). A cache that cannot cross within its patience
// at that rate is not worth having, so the size cap is the rate times the patience, and
// the same patience bounds the download, the upload and the packing that precedes it.
const CACHE_THROUGHPUT: u64 = 25 * MIB;
const CACHE_PATIENCE: Duration = Duration::from_secs(2 * 60);

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

    // Bounds what a host can put on its master's disk. A layer is a toolchain; a cache is
    // as much as can cross the link within its patience, so it is never stored only to be
    // too slow to fetch.
    pub(crate) fn max_bytes(self) -> u64 {
        match self {
            Self::Layer => 4 * GIB,
            Self::Cache => CACHE_THROUGHPUT * CACHE_PATIENCE.as_secs(),
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
            Self::Cache => CACHE_PATIENCE,
        }
    }

    // An upload moves the whole archive, so it gets minutes where a question gets
    // seconds, and it is still not left to the connection's ten-minute idle window. A
    // cache's is the same as its fetch: what is stored is what can be fetched.
    pub(crate) fn store_patience(self) -> Duration {
        match self {
            Self::Layer => Duration::from_secs(5 * 60),
            Self::Cache => CACHE_PATIENCE,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ArchiveRequest {
    pub token: String,
    pub kind: ArchiveKind,
    // What the host is: the image's own name, as the host declares it. A layer built
    // on one image is not the software another image's hosts expect to find.
    pub environment: String,
    // The archive's name for itself: the digest of the commands that build a layer, the
    // project's id for a cache.
    pub tag: String,
    pub op: ArchiveOp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum ArchiveOp {
    Fetch,
    // The archive follows the request, `len` bytes of it.
    Store { len: u64 },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum ArchiveReply {
    // The archive follows, `len` bytes of it.
    Blob { len: u64 },
    Missing,
    Stored,
    Failed(String),
}

// What a master keeps of what its hosts leave. Nothing here knows what an archive is:
// one per kind, environment and tag; layers age out like a base snapshot and caches only
// by the byte budget. A host is trusted to say which environment it is and which project
// a cache is for, because the token is the boundary on this link and everywhere else: a
// host holding it could already reach every task's channel, and a project's id is an
// unguessable UUID, so a host that lies only corrupts its own fleet's archives.
pub(crate) struct ArchiveStore {
    dir: PathBuf,
    caches: PathBuf,
    // Where a receive writes before it is an archive. Emptied when the store opens: a
    // file here is a receive that did not finish, and nothing can be receiving while
    // this process is starting.
    staging: PathBuf,
    token: String,
}

impl ArchiveStore {
    pub(crate) fn open(config: &Config, token: String) -> io::Result<Self> {
        let dir = config.data_dir().join("hosts").join("layers");
        let caches = dir.join("caches");
        let staging = dir.join("staging");
        for directory in [&staging, &caches] {
            fs::create_dir_all(directory).map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", directory.display()))
            })?;
        }
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
    fn staging_path(&self, request: &ArchiveRequest) -> PathBuf {
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
        if kind == ArchiveKind::Layer && age.is_some_and(|age| age >= LAYER_FRESHNESS) {
            // A layer this old is rebuilt by its host anyway, so it is dropped rather
            // than left to fill the disk.
            let _ = fs::remove_file(&path);
            return None;
        }
        Some((path, metadata.len()))
    }

    fn adopt(&self, request: &ArchiveRequest, received: PathBuf) -> io::Result<u64> {
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
pub(crate) struct ArchiveHandler {
    store: Arc<ArchiveStore>,
}

impl ArchiveHandler {
    pub(crate) fn new(store: Arc<ArchiveStore>) -> Self {
        Self { store }
    }
}

impl fmt::Debug for ArchiveHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ArchiveHandler")
    }
}

impl ProtocolHandler for ArchiveHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // A host asks for a layer and a cache per sandbox and leaves them on the way out,
        // all on the connection it holds, so each stream is answered as it arrives.
        loop {
            let Ok((send, recv)) = connection.accept_bi().await else {
                return Ok(());
            };
            let handler = self.clone();
            tokio::spawn(async move { handler.answer_archive(send, recv).await });
        }
    }
}

impl ArchiveHandler {
    async fn answer_archive(&self, mut send: SendStream, mut recv: RecvStream) {
        // A host waiting on an answer it never gets is indistinguishable from a master
        // that never heard the question, and only one of those is a bug here.
        let request: ArchiveRequest = match read_frame(&mut recv).await {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "an unreadable archive call arrived");
                return;
            }
        };
        tracing::info!(kind = request.kind.name(), tag = %request.tag, "a host called the archive store");
        if request.token != self.store.token() {
            let _ = write_frame(
                &mut send,
                &ArchiveReply::Failed("this host does not hold the master's token".into()),
            )
            .await;
            return;
        }
        match request.op {
            ArchiveOp::Fetch => {
                match self
                    .store
                    .fresh_archive(request.kind, &request.environment, &request.tag)
                {
                    Some((path, len)) => {
                        let Ok(mut archive) = AsyncFile::open(&path).await else {
                            let _ = write_frame(
                                &mut send,
                                &ArchiveReply::Failed("a stored archive could not be read".into()),
                            )
                            .await;
                            return;
                        };
                        if write_frame(&mut send, &ArchiveReply::Blob { len })
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
                        let _ = write_frame(&mut send, &ArchiveReply::Missing).await;
                    }
                }
            }
            ArchiveOp::Store { len } => {
                let reply = if len == 0 || len > request.kind.max_bytes() {
                    ArchiveReply::Failed(format!("{len} bytes is not a {}", request.kind.name()))
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
                                ArchiveReply::Stored
                            }
                            Err(error) => ArchiveReply::Failed(error.to_string()),
                        },
                        Err(error) => ArchiveReply::Failed(error),
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
