use std::{collections::HashMap, fmt, fs, io::Write, path::Path, sync::Arc, sync::Mutex};

use crate::remote::InstallationKey;
use crate::remote::Transfer;
use crate::remote::TransferPart;
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::Connection};
use tokio::fs::File as AsyncFile;
use tokio::io::copy;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::remote::layers::{LayerReply, LayerRequest};
use crate::remote::protocol::Conversation;
use crate::task::TaskAsk;
use crate::task::picture;

use crate::isolation::worker_protocol::{CommandFrame, WorkerReply, WorkerRequest};
use crate::isolation::{CommandEvent, CommandSession};

use crate::remote::protocol::SignalCommand;
use crate::remote::protocol::{
    BlobHash, CHUNK_BYTES, ConfigPush, Handover, LinkRequest, LinkResponse, TRANSFER_PIECE_BYTES,
    TaskRequest, TransferArchive, WatchEvent, WorkerReport, builder, read_body, read_frame,
    write_frame,
};
use crate::remote::transfer::{STOPPED, TransferUpdate, Unit};
use crate::remote::{
    LinkResult,
    bundle::ProfileDigest,
    discovery::{self, Scan},
};

mod path;
mod signals;
mod tasks;
mod workers;

use path::LinkPath;

// A peer and the conversation being held with it.
type Held = (EndpointId, Conversation);

#[derive(Clone)]
pub struct LinkClient {
    endpoint: Endpoint,
    // One connection per peer and conversation, kept for as long as it holds. A dial per
    // message is a handshake per message, and two handshakes to one peer at the same
    // moment — a heartbeat and a host's own layer fetch — is what starved a provision:
    // the second never got a usable path, so its stream was never sent.
    peers: Arc<Mutex<HashMap<Held, Connection>>>,
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

    // A conversation is what a dial starts: a client's tasks, a host's sandboxes, a
    // host's layers, a host's cache.
    pub(super) async fn dial(
        &self,
        peer: impl Into<EndpointAddr>,
        conversation: Conversation,
    ) -> LinkResult<Dial> {
        let peer = peer.into();
        let key = (peer.id, conversation);
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
        let connection = self.endpoint.connect(peer, conversation.alpn()).await?;
        self.peers
            .lock()
            .expect("the link's peer map")
            .insert(key, connection.clone());
        Ok(Dial(connection))
    }

    pub(super) async fn say(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
    ) -> LinkResult<LinkResponse> {
        let dial = self.dial(peer, Conversation::Link).await?;
        let (mut send, mut recv) = dial.0.open_bi().await?;
        write_frame(&mut send, request).await?;
        send.finish()?;
        let answer = read_frame(&mut recv).await;
        tracing::debug!(remote = %dial.remote(), path = %dial.path(), "spoke to a remote");
        answer
    }

    pub(super) async fn say_with_body(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
        body: &[u8],
    ) -> LinkResult<LinkResponse> {
        let dial = self.dial(peer, Conversation::Link).await?;
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

    pub(super) async fn ask_for_body(
        &self,
        peer: impl Into<EndpointAddr>,
        request: &LinkRequest,
    ) -> LinkResult<Vec<u8>> {
        let dial = self.dial(peer, Conversation::Link).await?;
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
}

pub(super) struct Dial(Connection);

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
