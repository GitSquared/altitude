//! The client side: dial a remote by key and run one exchange.

use std::{fmt, fs, io::Write, path::Path};

use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::Connection};

use super::{
    LinkResult,
    protocol::{
        BlobHash, CHUNK_BYTES, Handover, LINK_ALPN, LinkRequest, LinkResponse, TaskRequest,
        TaskSummary, TransferArchive, builder, read_body, read_frame, write_frame,
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
    pub async fn bind(identity: &super::InstallationKey) -> LinkResult<Self> {
        let endpoint = builder()
            .secret_key(identity.secret_key().clone())
            .bind()
            .await?;
        Ok(Self { endpoint })
    }

    pub async fn shutdown(&self) {
        self.endpoint.close().await;
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
            LinkResponse::TransferPiece { len } => {
                read_body(&mut recv, len, super::protocol::TRANSFER_PIECE_BYTES).await
            }
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

    pub async fn prepare_take(&self, peer: &EndpointAddr, task_id: &str) -> LinkResult<Handover> {
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

    pub async fn fetch_part(
        &self,
        peer: &EndpointAddr,
        transfer_id: &str,
        part: super::TransferPart,
        archive: &TransferArchive,
        landing: &Path,
    ) -> LinkResult<()> {
        if let Some(parent) = landing.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(landing)?;
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0;
        while offset < archive.len {
            let len = super::protocol::TRANSFER_PIECE_BYTES.min(archive.len - offset);
            let bytes = self
                .ask_for_body(
                    peer.clone(),
                    &LinkRequest::FetchTransferPart {
                        transfer_id: transfer_id.to_owned(),
                        part,
                        offset,
                        len,
                    },
                )
                .await?;
            if bytes.len() as u64 != len {
                return Err("the transfer piece was truncated".into());
            }
            hasher.update(&bytes);
            file.write_all(&bytes)?;
            offset += len;
        }
        file.sync_all()?;
        let arrived = hasher.finalize().to_hex().to_string();
        if arrived != archive.digest {
            return Err(format!("archive digest mismatch: {arrived}").into());
        }
        tracing::info!(%transfer_id, ?part, bytes = offset, "fetched a transfer archive");
        Ok(())
    }

    pub async fn confirm_take(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::HandedOver {
                    transfer_id: transfer_id.to_owned(),
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
