//! The client side: dial a remote by key and run one exchange.

// The dial side of the link. Taking a task over is reachable here before it is
// reachable from the interface, which is the next piece of work on remotes.
#![allow(dead_code)]
use std::{fmt, fs, io::Write, path::Path};

use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::Connection};

use super::{
    LinkResult,
    protocol::{
        BlobHash, CHUNK_BYTES, CapsuleArchive, Handover, LINK_ALPN, LinkRequest, LinkResponse,
        MAX_BLOB_BYTES, TaskRequest, TaskSummary, builder, read_body, read_frame, write_frame,
    },
};

// ------------------------------------------------------------- the client side

/// The kind of network path a connection is using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    /// A direct IP path.
    Direct,
    /// Traffic is going through a relay server.
    ///
    /// Unreachable while Altitude configures no relay, and kept because this
    /// mirrors what iroh itself reports rather than what we expect to see.
    Relay,
    /// No path was selected at the time it was observed.
    Unestablished,
}

/// How a connection travelled when the client looked, which is the difference
/// between "hole punching worked" and "we are paying a relay".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkPath {
    /// The path carrying application data.
    pub selected: PathKind,
    /// Whether a direct IP path was open as well. Reading relay with a direct
    /// path open means hole punching succeeded and iroh's deliberately sticky
    /// selector has not moved yet.
    pub direct_open: bool,
}

impl LinkPath {
    /// Observed after an exchange, which gives hole punching a chance to have
    /// replaced the relay path with a direct one.
    fn observe(connection: &Connection) -> Self {
        let paths = connection.paths();
        let selected = match paths.iter().find(|path| path.is_selected()) {
            Some(path) if path.is_relay() => PathKind::Relay,
            Some(_) => PathKind::Direct,
            None => PathKind::Unestablished,
        };
        Self {
            selected,
            direct_open: paths.iter().any(|path| path.is_ip()),
        }
    }
}

impl fmt::Display for LinkPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.selected, self.direct_open) {
            (PathKind::Direct, _) => f.write_str("direct"),
            (PathKind::Relay, true) => f.write_str("relay (direct path also open)"),
            (PathKind::Relay, false) => f.write_str("relay, no direct path"),
            (PathKind::Unestablished, _) => f.write_str("unestablished"),
        }
    }
}

/// A completed exchange, plus what the client could see about the peer.
pub struct LinkReply<T> {
    /// The answer itself.
    pub value: T,
    /// The key that answered. The handshake authenticates this, so it is the
    /// proof that dialling a public key reached that public key.
    pub remote: EndpointId,
    /// How the bytes travelled.
    pub path: LinkPath,
}

/// The client side of the link: an endpoint that dials remotes by public key.
pub struct LinkClient {
    endpoint: Endpoint,
}

impl LinkClient {
    /// Binds a client endpoint. Nothing about a remote is needed to do this.
    pub async fn bind() -> LinkResult<Self> {
        let endpoint = builder().bind().await?;
        Ok(Self { endpoint })
    }

    /// The client's own public key.
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Closes the endpoint.
    pub async fn close(self) -> LinkResult<()> {
        self.endpoint.close().await;
        Ok(())
    }

    /// Dials a remote and runs one request/response exchange.
    ///
    /// `peer` may be a bare [`EndpointId`] — no URL, no port — in which case
    /// the address has to be discoverable — a [`RemoteTicket`], or an
    /// [`EndpointAddr`].
    pub async fn request(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
    ) -> LinkResult<LinkReply<LinkResponse>> {
        let connection = self.endpoint.connect(peer, LINK_ALPN).await?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(&mut send, request).await?;
            send.finish()?;
            let value = read_frame(&mut recv).await?;
            Ok(LinkReply {
                value,
                remote: connection.remote_id(),
                path: LinkPath::observe(&connection),
            })
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    }

    /// Asks what tasks a remote owns.
    ///
    /// These are the only copies of them: a task belongs to the machine running it, so
    /// this is a view that is only as good as the answer.
    pub async fn list_tasks(&self, peer: impl Into<EndpointAddr>) -> LinkResult<Vec<TaskSummary>> {
        let connection = self.endpoint.connect(peer, LINK_ALPN).await?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(&mut send, &LinkRequest::ListTasks).await?;
            send.finish()?;
            let response: LinkResponse = read_frame(&mut recv).await?;
            match response {
                LinkResponse::Tasks { summaries } => Ok(summaries),
                LinkResponse::Rejected { reason } => Err(reason.into()),
                other => Err(format!("unexpected answer: {other:?}").into()),
            }
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    }

    /// Takes a task over from a remote, and returns what has to continue here.
    ///
    /// The remote stops running the task and bundles its capsule. This fetches the
    /// bundle in stretches, checks it against the digest the remote gave, and only then
    /// says it holds all of it. Until that moment the remote is still the owner, which
    /// is the failure that repairs itself: nobody runs the task for a moment, and the
    /// task is still where it was.
    pub async fn take_task(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        landing: &Path,
    ) -> LinkResult<Handover> {
        let peer = peer.into();
        let handover = self.ask_take(&peer, task_id).await?;
        if handover.capsule.len == 0 {
            self.confirm_take(&peer, task_id).await?;
            return Ok(handover);
        }
        self.fetch_capsule(&peer, task_id, &handover.capsule, landing)
            .await?;
        self.confirm_take(&peer, task_id).await?;
        Ok(handover)
    }

    /// How much of a capsule archive to ask for at a time.
    ///
    /// Large enough that a capsule is not thousands of round trips, and well inside
    /// what a single frame can carry.
    const CAPSULE_STRETCH: u64 = 4 << 20;

    async fn ask_take(&self, peer: &EndpointAddr, task_id: &str) -> LinkResult<Handover> {
        let connection = self.endpoint.connect(peer.clone(), LINK_ALPN).await?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(
                &mut send,
                &LinkRequest::TakeTask {
                    task_id: task_id.to_owned(),
                },
            )
            .await?;
            send.finish()?;
            match read_frame::<_, LinkResponse>(&mut recv).await? {
                LinkResponse::Handover { handover } => Ok(*handover),
                LinkResponse::Rejected { reason } => Err(reason.into()),
                other => Err(format!("unexpected answer: {other:?}").into()),
            }
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    }

    async fn fetch_capsule(
        &self,
        peer: &EndpointAddr,
        task_id: &str,
        capsule: &CapsuleArchive,
        landing: &Path,
    ) -> LinkResult<()> {
        if let Some(parent) = landing.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(landing)?;
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0;
        while offset < capsule.len {
            let len = Self::CAPSULE_STRETCH.min(capsule.len - offset);
            let connection = self.endpoint.connect(peer.clone(), LINK_ALPN).await?;
            let piece = async {
                let (mut send, mut recv) = connection.open_bi().await?;
                write_frame(
                    &mut send,
                    &LinkRequest::FetchCapsule {
                        task_id: task_id.to_owned(),
                        offset,
                        len,
                    },
                )
                .await?;
                send.finish()?;
                match read_frame::<_, LinkResponse>(&mut recv).await? {
                    LinkResponse::Capsule { len } => {
                        read_body(&mut recv, len, MAX_BLOB_BYTES).await
                    }
                    LinkResponse::Rejected { reason } => Err(reason.into()),
                    other => Err(format!("unexpected answer: {other:?}").into()),
                }
            }
            .await;
            connection.close(0u32.into(), b"done");
            let bytes = piece?;
            if bytes.is_empty() {
                break;
            }
            hasher.update(&bytes);
            file.write_all(&bytes)?;
            offset += bytes.len() as u64;
        }
        file.flush()?;
        let arrived = hasher.finalize().to_hex().to_string();
        if arrived != capsule.digest {
            return Err(format!(
                "the capsule arrived as {arrived} rather than {}",
                capsule.digest
            )
            .into());
        }
        Ok(())
    }

    async fn confirm_take(&self, peer: &EndpointAddr, task_id: &str) -> LinkResult<()> {
        let connection = self.endpoint.connect(peer.clone(), LINK_ALPN).await?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(
                &mut send,
                &LinkRequest::HandedOver {
                    task_id: task_id.to_owned(),
                },
            )
            .await?;
            send.finish()?;
            match read_frame::<_, LinkResponse>(&mut recv).await? {
                LinkResponse::Accepted => Ok(()),
                LinkResponse::Rejected { reason } => Err(reason.into()),
                other => Err(format!("unexpected answer: {other:?}").into()),
            }
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    }

    /// Asks a remote to run a task of its own, and returns when it has taken it.
    ///
    /// Nothing is copied: the task is founded there and belongs to that machine from
    /// the moment it exists. What the remote called it is read from the machine, not
    /// from this answer.
    pub async fn run_task(
        &self,
        peer: impl Into<EndpointAddr>,
        request: TaskRequest,
    ) -> LinkResult<()> {
        let connection = self.endpoint.connect(peer, LINK_ALPN).await?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(&mut send, &LinkRequest::RunTask { request }).await?;
            send.finish()?;
            let response: LinkResponse = read_frame(&mut recv).await?;
            match response {
                LinkResponse::Accepted => Ok(()),
                LinkResponse::Rejected { reason } => Err(reason.into()),
                other => Err(format!("unexpected answer: {other:?}").into()),
            }
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    }

    /// Sends a configuration, and returns when the remote holds it.
    pub async fn put_config(&self, peer: impl Into<EndpointAddr>, bytes: &[u8]) -> LinkResult<()> {
        let hash = BlobHash::of(bytes);
        let request = LinkRequest::PutConfig {
            hash,
            len: bytes.len() as u64,
        };
        let connection = self.endpoint.connect(peer, LINK_ALPN).await?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(&mut send, &request).await?;
            for chunk in bytes.chunks(CHUNK_BYTES) {
                send.write_all(chunk).await?;
            }
            send.finish()?;
            let response: LinkResponse = read_frame(&mut recv).await?;
            match response {
                LinkResponse::Configured => Ok(()),
                LinkResponse::Rejected { reason } => Err(reason.into()),
                other => Err(format!("unexpected answer: {other:?}").into()),
            }
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    }

    /// Sends a blob and returns the digest the remote confirmed it received.
    ///
    /// The client hashes what it sends; the remote hashes what it receives and
    /// answers with the digest only when the two agree.
    pub async fn put_blob(
        &self,
        peer: impl Into<EndpointAddr>,
        bytes: &[u8],
    ) -> LinkResult<LinkReply<BlobHash>> {
        let hash = BlobHash::of(bytes);
        let request = LinkRequest::PutBlob {
            hash: hash.clone(),
            len: bytes.len() as u64,
        };
        let connection = self.endpoint.connect(peer, LINK_ALPN).await?;
        let result = async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_frame(&mut send, &request).await?;
            for chunk in bytes.chunks(CHUNK_BYTES) {
                send.write_all(chunk).await?;
            }
            send.finish()?;
            let response: LinkResponse = read_frame(&mut recv).await?;
            let value = match response {
                LinkResponse::Stored {
                    hash: confirmed, ..
                } if confirmed == hash => confirmed,
                LinkResponse::Rejected { reason } => return Err(reason.into()),
                other => return Err(format!("unexpected answer: {other:?}").into()),
            };
            Ok(LinkReply {
                value,
                remote: connection.remote_id(),
                path: LinkPath::observe(&connection),
            })
        }
        .await;
        connection.close(0u32.into(), b"done");
        result
    }
}
