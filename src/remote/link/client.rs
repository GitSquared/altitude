use std::{collections::HashMap, fmt, fs, io::Write, path::Path, sync::Arc, sync::Mutex};

use crate::remote::InstallationKey;
use crate::remote::Transfer;
use crate::remote::TransferPart;
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::Connection};
use tokio::fs::File as AsyncFile;
use tokio::io::copy;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::isolation::worker_protocol::WORKER_ALPN;
#[cfg(target_os = "linux")]
use crate::remote::cache::protocol::{CACHE_ALPN, CacheReply, CacheRequest};
use crate::remote::layers::{LAYER_ALPN, LayerReply, LayerRequest};
#[cfg(target_os = "linux")]
use crate::remote::protocol::MAX_BLOB_BYTES;
use crate::task::TaskAsk;
use crate::task::picture;

use crate::isolation::worker_protocol::{CommandFrame, WorkerReply, WorkerRequest};
use crate::isolation::{CommandEvent, CommandSession};

use crate::remote::protocol::SignalCommand;
use crate::remote::protocol::{
    BlobHash, CHUNK_BYTES, ConfigPush, Handover, LINK_ALPN, LinkRequest, LinkResponse,
    TRANSFER_PIECE_BYTES, TaskRequest, TransferArchive, WatchEvent, WorkerReport, builder,
    read_body, read_frame, write_frame,
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

// A peer and the conversation being held with it.
type Conversation = (EndpointId, Vec<u8>);

#[derive(Clone)]
pub struct LinkClient {
    endpoint: Endpoint,
    // One connection per peer and conversation, kept for as long as it holds. A dial per
    // message is a handshake per message, and two handshakes to one peer at the same
    // moment — a heartbeat and a host's own layer fetch — is what starved a provision:
    // the second never got a usable path, so its stream was never sent.
    peers: Arc<Mutex<HashMap<Conversation, Connection>>>,
}

impl LinkClient {
    pub async fn bind(identity: &InstallationKey) -> LinkResult<Self> {
        let endpoint = builder()
            .secret_key(identity.secret_key().clone())
            .bind()
            .await?;
        Ok(Self {
            endpoint,
            peers: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    // An endpoint that also serves: the worker answers the master's calls on the
    // same endpoint it registers over.
    pub(crate) fn from_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            peers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn scan_remotes(&self) -> LinkResult<Scan> {
        discovery::scan(&self.endpoint).await
    }

    // One cache call to a master, carrying the body a write needs and returning the
    // bytes a read produced.
    #[cfg(target_os = "linux")]
    pub(crate) async fn cache_call(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &CacheRequest,
        body: &[u8],
    ) -> LinkResult<(CacheReply, Vec<u8>)> {
        // The cache answers on its own ALPN, not the link's: a master serves three
        // conversations, and a host that dials the wrong one is refused at the
        // handshake rather than told to try again.
        let dial = self.dial_alpn(peer, CACHE_ALPN).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        if !body.is_empty() {
            send.write_all(body).await?;
        }
        send.finish()?;
        let reply: CacheReply = read_frame(&mut recv).await?;
        let bytes = match &reply {
            CacheReply::Data { len } => {
                read_body(&mut recv, u64::from(*len), MAX_BLOB_BYTES).await?
            }
            _ => Vec::new(),
        };
        Ok((reply, bytes))
    }

    // What another host built, fetched by this one: the archive is copied into the
    // sink, and `false` says the master has nothing for this environment and tag.
    pub(crate) async fn layer_fetch(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LayerRequest,
        mut sink: AsyncFile,
    ) -> LinkResult<bool> {
        let dial = self.dial_alpn(peer, LAYER_ALPN).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        match read_frame::<_, LayerReply>(&mut recv).await? {
            LayerReply::Blob { .. } => {
                copy(&mut recv, &mut sink).await?;
                sink.sync_all().await?;
                Ok(true)
            }
            LayerReply::Missing => Ok(false),
            LayerReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // This host's copy, left for the next one. A failure is the caller's to log and
    // forget: a layer that cannot be shared is a layer the next host builds itself.
    pub(crate) async fn layer_store(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LayerRequest,
        mut archive: AsyncFile,
    ) -> LinkResult<()> {
        let dial = self.dial_alpn(peer, LAYER_ALPN).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        copy(&mut archive, &mut send).await?;
        send.finish()?;
        match read_frame::<_, LayerReply>(&mut recv).await? {
            LayerReply::Stored => Ok(()),
            LayerReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // One sandbox call to a worker the master provisioned.
    pub(crate) async fn worker_call(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
    ) -> LinkResult<WorkerReply> {
        let dial = self.dial_alpn(peer, WORKER_ALPN).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        read_frame(&mut recv).await
    }

    // A provision carries the operator's profiles, which are too large for a frame:
    // they follow it as the rest of the stream, the way a delivered file does.
    pub(crate) async fn worker_provision(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
        body: &[u8],
    ) -> LinkResult<WorkerReply> {
        let dial = self.dial_alpn(peer, WORKER_ALPN).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.write_all(body).await?;
        send.finish()?;
        read_frame(&mut recv).await
    }

    // A host carries one connection from an agent inside its sandbox to the
    // channel the master serves for that task.
    pub(crate) async fn pipe_agent_channel(
        &self,
        peer: impl Into<EndpointAddr>,
        socket: &str,
        token: &str,
        tcp: TcpStream,
    ) -> LinkResult<()> {
        let dial = self.dial(peer).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(
            &mut send,
            &LinkRequest::AgentChannel {
                socket: socket.to_owned(),
                token: token.to_owned(),
            },
        )
        .await?;
        match read_frame::<_, LinkResponse>(&mut recv).await? {
            LinkResponse::Accepted => {}
            LinkResponse::Rejected { reason } => return Err(reason.into()),
            other => return Err(format!("unexpected answer: {other:?}").into()),
        }
        let (mut box_read, mut box_write) = tcp.into_split();
        let to_master = async {
            let _ = copy(&mut box_read, &mut send).await;
            let _ = send.finish();
        };
        let to_box = async {
            let _ = copy(&mut recv, &mut box_write).await;
        };
        tokio::join!(to_master, to_box);
        Ok(())
    }

    // Send a file's bytes to a worker on the stream that carries the call.
    pub(crate) async fn worker_deliver(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
        mut file: AsyncFile,
    ) -> LinkResult<()> {
        let dial = self.dial_alpn(peer, WORKER_ALPN).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        copy(&mut file, &mut send).await?;
        send.finish()?;
        match read_frame::<_, WorkerReply>(&mut recv).await? {
            WorkerReply::Ok => Ok(()),
            WorkerReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // Receive a collected file or tree on the stream the call opened.
    pub(crate) async fn worker_collect(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
        mut sink: AsyncFile,
    ) -> LinkResult<(u64, u64)> {
        let dial = self.dial_alpn(peer, WORKER_ALPN).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        match read_frame::<_, WorkerReply>(&mut recv).await? {
            WorkerReply::Blob { files, bytes, .. } => {
                copy(&mut recv, &mut sink).await?;
                Ok((files, bytes))
            }
            WorkerReply::Failed(reason) => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    // A command on a worker keeps its stream open: input goes one way and events
    // the other until the command exits.
    pub(crate) async fn worker_command(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &WorkerRequest,
    ) -> LinkResult<CommandSession> {
        let dial = self.dial(peer).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        match read_frame::<_, WorkerReply>(&mut recv).await? {
            WorkerReply::Ready => {}
            WorkerReply::Failed(reason) => return Err(reason.into()),
            other => return Err(format!("unexpected answer: {other:?}").into()),
        }
        let (commands, mut wanted) = mpsc::unbounded_channel();
        let (producing, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            // The connection lives as long as the command does.
            let _dial = dial;
            loop {
                tokio::select! {
                    command = wanted.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        if write_frame(&mut send, &CommandFrame::Drive(command)).await.is_err() {
                            break;
                        }
                    }
                    frame = read_frame::<_, CommandFrame>(&mut recv) => {
                        match frame {
                            Ok(CommandFrame::Report(event)) => {
                                let done = matches!(event, CommandEvent::Exited(_) | CommandEvent::Failed(_));
                                if producing.send(event).is_err() {
                                    break;
                                }
                                if done {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }
                }
            }
            let _ = send.finish();
        });
        Ok(CommandSession { commands, events })
    }

    // A provisioned worker tying itself to the master it was made for. The token
    // is the only thing that says the connection is wanted.
    pub async fn register_worker(
        &self,
        peer: impl Into<EndpointAddr>,
        addr: EndpointAddr,
        token: &str,
        name: &str,
        capacity: WorkerReport,
    ) -> LinkResult<()> {
        match self
            .say(
                peer,
                &LinkRequest::RegisterWorker {
                    token: token.to_owned(),
                    name: name.to_owned(),
                    capacity,
                    addr,
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn worker_heartbeat(
        &self,
        peer: impl Into<EndpointAddr>,
        name: &str,
    ) -> LinkResult<()> {
        match self
            .say(
                peer,
                &LinkRequest::WorkerHeartbeat {
                    name: name.to_owned(),
                },
            )
            .await?
        {
            LinkResponse::Accepted => Ok(()),
            LinkResponse::Rejected { reason } => Err(reason.into()),
            other => Err(format!("unexpected answer: {other:?}").into()),
        }
    }

    pub async fn shutdown(&self) {
        self.endpoint.close().await;
    }

    async fn dial(&self, peer: impl Into<EndpointAddr>) -> LinkResult<Dial> {
        self.dial_alpn(peer, LINK_ALPN).await
    }

    // Which conversation a dial starts is the ALPN, and the three of them are
    // different conversations: a client's tasks, a host's sandboxes, a host's layers.
    async fn dial_alpn(&self, peer: impl Into<EndpointAddr>, alpn: &[u8]) -> LinkResult<Dial> {
        let peer = peer.into();
        let key = (peer.id, alpn.to_vec());
        if let Some(held) = self
            .peers
            .lock()
            .expect("the link's peer map")
            .get(&key)
            .cloned()
            && held.close_reason().is_none()
        {
            return Ok(Dial(held));
        }
        let connection = self.endpoint.connect(peer, alpn).await?;
        self.peers
            .lock()
            .expect("the link's peer map")
            .insert(key, connection.clone());
        Ok(Dial(connection))
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
    ) -> LinkResult<Vec<TaskAsk>> {
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

// The connection outlives this exchange: it is the peer's connection, kept until the
// peer goes or the endpoint does, and closed with it.
