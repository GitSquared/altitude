//! The client side: dial a remote by key and run one exchange.

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

/// The client side of the link: an endpoint that dials remotes by public key.
///
/// Cloning shares the one endpoint. Binding one per exchange would give this machine a
/// new identity to every remote it speaks to, which is both a handshake each time and a
/// stranger each time: a remote that keeps a record of its clients would see a new one
/// every few seconds.
#[derive(Clone)]
pub struct LinkClient {
    endpoint: Endpoint,
}

impl LinkClient {
    /// Binds a client endpoint. Nothing about a remote is needed to do this.
    pub async fn bind() -> LinkResult<Self> {
        let endpoint = builder().bind().await?;
        Ok(Self { endpoint })
    }

    /// Dials a remote, and closes the connection however the exchange ends.
    async fn dial(&self, peer: impl Into<EndpointAddr>) -> LinkResult<Dial> {
        Ok(Dial(self.endpoint.connect(peer, LINK_ALPN).await?))
    }

    /// The one exchange every operation is: say one thing, hear one thing.
    ///
    /// Each operation differs only in what follows the frame and whether the answer has
    /// a body of its own, so this is where a connection is opened and closed and where
    /// the answer says how it travelled.
    async fn say(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
    ) -> LinkResult<LinkResponse> {
        let dial = self.dial(peer).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        let answer = read_frame(&mut recv).await;
        tracing::debug!(remote = %dial.remote(), path = %dial.path(), "spoke to a remote");
        answer
    }

    /// Say one thing, send a body after it, and hear one answer.
    async fn say_with_body(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
        body: &[u8],
    ) -> LinkResult<LinkResponse> {
        let dial = self.dial(peer).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        for chunk in body.chunks(CHUNK_BYTES) {
            send.write_all(chunk).await?;
        }
        send.finish()?;
        let answer = read_frame(&mut recv).await;
        tracing::debug!(remote = %dial.remote(), path = %dial.path(), "spoke to a remote");
        answer
    }

    /// Say one thing, and read the body of the answer.
    async fn ask_for_body(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
    ) -> LinkResult<Vec<u8>> {
        let dial = self.dial(peer).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        match read_frame::<_, LinkResponse>(&mut recv).await? {
            LinkResponse::Capsule { len } => read_body(&mut recv, len, MAX_BLOB_BYTES).await,
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    /// Asks what tasks a remote owns.
    pub async fn list_tasks(&self, peer: impl Into<EndpointAddr>) -> LinkResult<Vec<TaskSummary>> {
        match self.say(peer, &LinkRequest::ListTasks).await? {
            LinkResponse::Tasks { summaries } => Ok(summaries),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
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
        match self
            .say(
                peer.clone(),
                &LinkRequest::TakeTask {
                    task_id: task_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Handover { handover } => Ok(*handover),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
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
            let bytes = self
                .ask_for_body(
                    peer.clone(),
                    &LinkRequest::FetchCapsule {
                        task_id: task_id.to_owned(),
                        offset,
                        len,
                    },
                )
                .await?;
            if bytes.is_empty() {
                break;
            }
            hasher.update(&bytes);
            file.write_all(&bytes)?;
            offset += bytes.len() as u64;
        }
        file.flush()?;
        let arrived = hasher.finalize().to_hex().to_string();
        tracing::info!(task = %task_id, bytes = offset, "fetched a capsule");
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
        match self
            .say(
                peer.clone(),
                &LinkRequest::HandedOver {
                    task_id: task_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    /// Asks a remote to run a task of its own, and returns when it has taken it.
    ///
    /// Nothing is copied: the task is founded there and belongs to that machine from
    /// the moment it accepts, which is why the answer is an acceptance and not a receipt.
    pub async fn run_task(
        &self,
        peer: impl Into<EndpointAddr>,
        request: TaskRequest,
    ) -> LinkResult<()> {
        match self.say(peer, &LinkRequest::RunTask { request }).await? {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    /// Sends this machine's configuration for a remote to build from.
    pub async fn put_config(&self, peer: impl Into<EndpointAddr>, bytes: &[u8]) -> LinkResult<()> {
        let request = LinkRequest::PutConfig {
            hash: BlobHash::of(bytes),
            len: bytes.len() as u64,
        };
        match self.say_with_body(peer, &request, bytes).await? {
            LinkResponse::Configured => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }
}

/// A connection to a remote, closed when it goes out of scope.
///
/// Every exchange opens one, and the ones that forget leave it up until the endpoint
/// notices. Closing here means no operation has to remember to.
struct Dial(Connection);

impl Dial {
    /// The key that answered, which the handshake has authenticated.
    fn remote(&self) -> EndpointId {
        self.0.remote_id()
    }

    /// How the bytes travelled, which is the difference between hole punching working
    /// and paying for a relay.
    fn path(&self) -> LinkPath {
        LinkPath::observe(&self.0)
    }
}

impl Drop for Dial {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"done");
    }
}
