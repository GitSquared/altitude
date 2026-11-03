use std::{fmt, fs, io::Write, path::Path};

use crate::remote::InstallationKey;
use crate::remote::Transfer;
use crate::remote::TransferPart;
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::Connection};
use tokio::sync::mpsc;

use crate::task::TaskOperatorSignalThread;
use crate::task::picture;

use crate::remote::protocol::SignalCommand;
use crate::remote::protocol::{
    BlobHash, CHUNK_BYTES, ConfigPush, Handover, LINK_ALPN, LinkRequest, LinkResponse,
    TRANSFER_PIECE_BYTES, TaskRequest, TransferArchive, WatchEvent, builder, read_body, read_frame,
    write_frame,
};
use crate::remote::transfer::{STOPPED, TransferUpdate, Unit};
use crate::remote::{
    LinkResult,
    bundle::ProfileDigest,
    discovery::{self, Scan},
};

// ------------------------------------------------------------- the client side

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    Direct,
    Relay,
    Unestablished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkPath {
    pub selected: PathKind,
    pub direct_open: bool,
}

impl LinkPath {
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

#[derive(Clone)]
pub struct LinkClient {
    endpoint: Endpoint,
}

impl LinkClient {
    pub async fn bind(identity: &InstallationKey) -> LinkResult<Self> {
        let endpoint = builder()
            .secret_key(identity.secret_key().clone())
            .bind()
            .await?;
        Ok(Self { endpoint })
    }

    pub async fn scan_remotes(&self) -> LinkResult<Scan> {
        discovery::scan(&self.endpoint).await
    }

    pub async fn shutdown(&self) {
        self.endpoint.close().await;
    }

    async fn dial(&self, peer: impl Into<EndpointAddr>) -> LinkResult<Dial> {
        Ok(Dial(self.endpoint.connect(peer, LINK_ALPN).await?))
    }

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
                read_body(&mut recv, len, TRANSFER_PIECE_BYTES).await
            }
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn watch_tasks(
        &self,
        peer: impl Into<EndpointAddr>,
        events: mpsc::UnboundedSender<WatchEvent>,
    ) -> LinkResult<()> {
        let dial = self.dial(peer).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, &LinkRequest::WatchTasks).await?;
        send.finish()?;
        loop {
            let answer: LinkResponse = read_frame(&mut recv).await?;
            let event = match answer {
                LinkResponse::Tasks { summaries } => WatchEvent::Snapshot(summaries),
                LinkResponse::TaskChanged { summary } => WatchEvent::Changed(summary),
                LinkResponse::TaskRemoved { id } => WatchEvent::Removed(id),
                LinkResponse::Ready => WatchEvent::Ready,
                LinkResponse::Rejected { reason } => return Err(reason.into()),
                other => return Err(format!("unexpected answer: {other:?}").into()),
            };
            if events.send(event).is_err() {
                // Nobody is reading: the watch ends with its reader.
                return Ok(());
            }
        }
    }

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

    pub async fn take_progress(
        &self,
        peer: &EndpointAddr,
        task_id: &str,
    ) -> Option<TransferUpdate> {
        let answer = self
            .say(
                peer.clone(),
                &LinkRequest::TakeProgress {
                    task_id: task_id.to_owned(),
                },
            )
            .await
            .ok()?;
        match answer {
            LinkResponse::Collecting {
                stage,
                done,
                total,
                unit,
            } => Some(TransferUpdate {
                stage,
                done,
                total,
                unit: if unit == "files" {
                    Unit::Files
                } else {
                    Unit::Bytes
                },
            }),
            _ => None,
        }
    }

    pub async fn fetch_part(
        &self,
        peer: &EndpointAddr,
        transfer_id: &str,
        part: TransferPart,
        archive: &TransferArchive,
        landing: &Path,
        transfer: &Transfer,
    ) -> LinkResult<()> {
        if let Some(parent) = landing.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(landing)?;
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0;
        while offset < archive.len {
            if transfer.is_stopped() {
                return Err(STOPPED.into());
            }
            let len = TRANSFER_PIECE_BYTES.min(archive.len - offset);
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

    pub async fn prepare_import(&self, peer: &EndpointAddr, handover: &Handover) -> LinkResult<()> {
        match self
            .say(
                peer.clone(),
                &LinkRequest::PrepareImport {
                    handover: Box::new(handover.clone()),
                },
            )
            .await?
        {
            LinkResponse::ImportPrepared => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn put_import_part(
        &self,
        peer: &EndpointAddr,
        transfer_id: &str,
        part: TransferPart,
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

    // The bytes a digest names, read from the machine that holds them.
    pub async fn picture(&self, peer: impl Into<EndpointAddr>, id: &str) -> LinkResult<Vec<u8>> {
        let dial = self.dial(peer).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, &LinkRequest::ReadPicture { id: id.to_owned() }).await?;
        send.finish()?;
        match read_frame::<_, LinkResponse>(&mut recv).await? {
            LinkResponse::Picture { id: answer, len } if answer == id => {
                read_body(&mut recv, len, picture::LARGEST).await
            }
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn signal_threads(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
    ) -> LinkResult<Vec<TaskOperatorSignalThread>> {
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
        command: SignalCommand,
    ) -> LinkResult<()> {
        match self.say(peer, &LinkRequest::Signal { command }).await? {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn express_input(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        body: &str,
    ) -> LinkResult<()> {
        match self
            .say(
                peer,
                &LinkRequest::Input {
                    task_id: task_id.into(),
                    body: body.into(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn reply_operator_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        signal_id: &str,
        body: &str,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            SignalCommand::Reply {
                task_id: task_id.into(),
                signal_id: signal_id.into(),
                body: body.into(),
            },
        )
        .await
    }

    pub async fn resolve_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        signal_id: &str,
        resolved: bool,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            SignalCommand::Resolve {
                task_id: task_id.into(),
                signal_id: signal_id.into(),
                resolved,
            },
        )
        .await
    }

    pub async fn confirm_operator_signal(
        &self,
        peer: impl Into<EndpointAddr>,
        task_id: &str,
        signal_id: &str,
    ) -> LinkResult<()> {
        self.signal(
            peer,
            SignalCommand::Accept {
                task_id: task_id.into(),
                signal_id: signal_id.into(),
            },
        )
        .await
    }
    pub async fn put_config(
        &self,
        peer: impl Into<EndpointAddr>,
        bytes: &[u8],
        take_tracker: bool,
    ) -> LinkResult<ConfigPush> {
        let request = LinkRequest::PutConfig {
            hash: BlobHash::of(bytes),
            len: bytes.len() as u64,
            take_tracker,
        };
        match self.say_with_body(peer, &request, bytes).await? {
            LinkResponse::Configured => Ok(ConfigPush::Stored),
            LinkResponse::TrackerLinkedElsewhere {
                provider,
                display_name,
            } => Ok(ConfigPush::TrackerLinkedElsewhere {
                provider,
                display_name,
            }),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }
}

struct Dial(Connection);

impl Dial {
    fn remote(&self) -> EndpointId {
        self.0.remote_id()
    }

    fn path(&self) -> LinkPath {
        LinkPath::observe(&self.0)
    }
}

impl Drop for Dial {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"done");
    }
}
