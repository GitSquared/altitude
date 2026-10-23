//! The client side: dial a remote by key and run one exchange.

use std::{fmt, fs, io::Write, path::Path};

use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::Connection};
use tokio::sync::mpsc;

use crate::task::TaskOperatorSignal;

use super::{
    LinkResult,
    bundle::ProfileDigest,
    discovery::{self, Scan},
    protocol::{
        BlobHash, CHUNK_BYTES, Handover, LINK_ALPN, LinkRequest, LinkResponse, TaskEvent,
        TaskRequest, TransferArchive, builder, read_body, read_frame, write_frame,
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

    /// Combines available network sources into remote suggestions.
    /// The scan ends after five seconds; dropping it cancels in-flight probes.
    pub async fn scan_remotes(&self) -> LinkResult<Scan> {
        discovery::scan(&self.endpoint).await
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

    /// Watches a machine's tasks: what it holds now, and every change after.
    ///
    /// The first frame is the snapshot and the rest are changes, so a watch that
    /// reconnects re-syncs itself and nobody has to track a gap. Returning means
    /// the watch ended, either because the machine stopped answering or because
    /// the reader went away.
    pub async fn watch_tasks(
        &self,
        peer: impl Into<EndpointAddr>,
        events: mpsc::UnboundedSender<TaskEvent>,
    ) -> LinkResult<()> {
        let dial = self.dial(peer).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, &LinkRequest::WatchTasks).await?;
        send.finish()?;
        loop {
            let answer: LinkResponse = read_frame(&mut recv).await?;
            let event = match answer {
                LinkResponse::Tasks { summaries } => TaskEvent::Snapshot(summaries),
                LinkResponse::TaskChanged { summary } => TaskEvent::Changed(summary),
                LinkResponse::TaskRemoved { id } => TaskEvent::Removed(id),
                LinkResponse::Rejected { reason } => return Err(reason.into()),
                other => return Err(format!("unexpected answer: {other:?}").into()),
            };
            if events.send(event).is_err() {
                // Nobody is reading: the watch ends with its reader.
                return Ok(());
            }
        }
    }

    /// Removes a task from the machine that holds it, disposing of its capsule.
    ///
    /// The same act as removing a task here, performed by the machine it belongs
    /// to: a remote is compute, not a different kind of task.
    pub async fn remove_task(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
    ) -> LinkResult<()> {
        match self
            .say(
                peer,
                &LinkRequest::RemoveTask {
                    task_id: task_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Removed => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    /// Asks which profiles a remote already holds for this client.
    ///
    /// Answered from the authenticated peer, so it is this client's stamps and not
    /// whatever the remote holds for everyone else.
    pub async fn held_profiles(
        &self,
        peer: impl Into<EndpointAddr>,
    ) -> LinkResult<Vec<ProfileDigest>> {
        match self.say(peer, &LinkRequest::HeldProfiles).await? {
            LinkResponse::HeldProfiles { profiles } => Ok(profiles),
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

    /// Fetches one archive, counting as it lands and stopping when asked.
    pub async fn fetch_part(
        &self,
        peer: &EndpointAddr,
        transfer_id: &str,
        part: super::TransferPart,
        archive: &TransferArchive,
        landing: &Path,
        transfer: &super::Transfer,
    ) -> LinkResult<()> {
        if let Some(parent) = landing.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(landing)?;
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0;
        while offset < archive.len {
            if transfer.is_stopped() {
                return Err(super::transfer::STOPPED.into());
            }
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
            transfer.advanced(len);
        }
        file.sync_all()?;
        let arrived = hasher.finalize().to_hex().to_string();
        if arrived != archive.digest {
            return Err(format!("archive digest mismatch: {arrived}").into());
        }
        tracing::info!(%transfer_id, ?part, bytes = offset, "fetched a transfer archive");
        Ok(())
    }

    /// Gives up a prepared transfer, so the machine that owns the task takes it
    /// back rather than holding it stopped.
    pub async fn abandon_take(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::AbandonTake {
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

    /// Gives up a staged import, so a stopped send leaves nothing half-written.
    pub async fn abandon_import(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::AbandonImport {
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

    /// Offer a stopped task to a remote, and learn whether it needs the base
    /// environment the capsule was taken against.
    pub async fn prepare_import(
        &self,
        peer: &EndpointAddr,
        handover: &Handover,
    ) -> LinkResult<bool> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::PrepareImport {
                    handover: Box::new(handover.clone()),
                },
            )
            .await?
        {
            LinkResponse::ImportPrepared { needs_environment } => Ok(needs_environment),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    /// Send one bounded piece of an archive to the remote staging it.
    pub async fn put_import_part(
        &self,
        peer: &EndpointAddr,
        transfer_id: &str,
        part: super::TransferPart,
        offset: u64,
        bytes: &[u8],
    ) -> LinkResult<()> {
        let request = LinkRequest::PutImportPart {
            transfer_id: transfer_id.to_owned(),
            part,
            offset,
            hash: BlobHash::of(bytes),
            len: bytes.len() as u64,
        };
        match self.say_with_body(peer.clone(), &request, bytes).await? {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    /// Ask the remote to land and boot the task it has staged, returning the id
    /// it gave it there.
    pub async fn stage_import(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<String> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::StageImport {
                    transfer_id: transfer_id.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Imported { task_id } => Ok(task_id),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    /// Tell the remote the source has relinquished ownership, so the landed task
    /// may begin. Idempotent: a lost answer is retried by a restarted source.
    pub async fn commit_import(&self, peer: &EndpointAddr, transfer_id: &str) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::CommitImport {
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
        let bytes = serde_json::to_vec(&request)?;
        let request = LinkRequest::RunTask {
            hash: BlobHash::of(&bytes),
            len: bytes.len() as u64,
        };
        match self.say_with_body(peer, &request, &bytes).await? {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn operator_signals(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
    ) -> LinkResult<Vec<TaskOperatorSignal>> {
        match self
            .say(
                peer,
                &LinkRequest::ListSignals {
                    task_id: task_id.into(),
                },
            )
            .await?
        {
            LinkResponse::Signals { signals } => Ok(signals),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    async fn signal(
        &self,
        peer: impl Into<EndpointAddr>,
        command: super::protocol::SignalCommand,
    ) -> LinkResult<()> {
        match self.say(peer, &LinkRequest::Signal { command }).await? {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn read_operator_signals(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            super::protocol::SignalCommand::Read {
                task_id: task_id.into(),
            },
        )
        .await
    }
    pub async fn acknowledge_operator_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        signal_id: &str,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            super::protocol::SignalCommand::Acknowledge {
                signal_id: signal_id.into(),
            },
        )
        .await
    }
    pub async fn answer_operator_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        signal_id: &str,
        answer: &str,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            super::protocol::SignalCommand::Answer {
                signal_id: signal_id.into(),
                answer: answer.into(),
            },
        )
        .await
    }
    pub async fn confirm_operator_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        signal_id: &str,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            super::protocol::SignalCommand::Accept {
                signal_id: signal_id.into(),
            },
        )
        .await
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
