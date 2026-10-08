//! The wire contract, and the vocabulary both sides share.
//!
//! Frames are a big-endian u32 length followed by that many bytes of JSON. Blob
//! bytes never travel inside a frame; a frame announces them and `len` raw bytes
//! follow on the same stream.
//!
//! This is also where the thing both sides need to agree on lives: how a peer is
//! addressed ([`RemoteTicket`]).

use std::{fmt, net::Ipv6Addr, str::FromStr};

use tokio::net::lookup_host;

use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Builder, presets},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::agent::AgentKind;
use crate::task::{TaskExecutionMode, TaskExecutionStatus, TaskHistoryEvent};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{
    LinkError, LinkResult,
    announce::{self, Announcement},
};

/// The one ALPN this protocol speaks. Two endpoints that do not agree on this
/// string never complete a handshake.
pub const LINK_ALPN: &[u8] = b"altitude/link/1";

/// Upper bound on a framed message. Requests are small; blob bytes travel
/// outside the frame.
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Upper bound on a single blob, so a peer cannot make a remote allocate
/// without limit. The capsule sizes this has to carry are not yet known.
pub(super) const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
/// Write granularity for blob bytes.
pub(super) const CHUNK_BYTES: usize = 64 * 1024;

/// The endpoint both sides bind.
///
/// No relay, no address lookup: an endpoint reaches exactly the addresses it was
/// given. That is the point — it keeps a third party out of both the data path
/// and the addressing path, and it matches how a remote is meant to be reached: on
/// a network you already share, or at an address the operator supplied.
///
/// What it gives up is hole punching, so two peers that are both behind NAT and
/// share no network cannot reach each other. A relay would come back as an
/// explicitly configured, self-hosted server, never an inherited default.
pub(super) fn builder() -> Builder {
    Endpoint::builder(presets::Minimal)
}

// ------------------------------------------------------------------- the wire

/// A BLAKE3 digest, as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobHash(String);

impl BlobHash {
    /// Hashes a buffer.
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One request. A blob's bytes follow the frame on the same stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LinkRequest {
    /// Ask the remote who it is. The answer is authenticated by the handshake,
    /// so it is really a check that both sides agree on the key.
    Identify,
    /// Ask the remote to run a task of its own.
    ///
    /// A task belongs to the machine that runs it, so this is not a copy of anything:
    /// the remote founds the task, keeps it, and is the only machine that can say
    /// what it is doing. The client is told the id so it can ask again later.
    RunTask { request: TaskRequest },
    /// Ask what tasks the remote owns.
    ListTasks,
    /// Ask the remote to hand a task over.
    ///
    /// The machine running a task is the machine that owns it, so this is the whole of
    /// taking one over: the source stops running it, the capsule travels, and the
    /// destination owns it once it holds all of it. Nothing is copied on the way.
    TakeTask { task_id: String },
    /// Ask for a stretch of a handed-over capsule, as an offset into its archive.
    FetchCapsule {
        task_id: String,
        offset: u64,
        len: u64,
    },
    /// The destination holds all of it, which is the last thing a transfer needs.
    /// Without it the task stays where it was rather than nowhere.
    HandedOver { task_id: String },
    /// Announce an upload: `len` raw bytes follow, whose digest must be `hash`.
    PutBlob { hash: BlobHash, len: u64 },
    /// Announce a configuration: `len` raw bytes follow, whose digest must be
    /// `hash`. It is what this client runs tasks with, so a remote that holds it
    /// can build the same environments.
    PutConfig { hash: BlobHash, len: u64 },
}

/// The remote's answer to a [`LinkRequest`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LinkResponse {
    /// Who the remote is.
    Identified { remote: String },
    /// The upload arrived and its digest matched.
    Stored { hash: BlobHash, len: u64 },
    /// The configuration arrived, and the remote holds it now.
    Configured,
    /// The remote took the task. What it called it is read from the machine rather
    /// than returned, because the machine is where the task is.
    Accepted,
    /// What the remote owns, as its own cards would show it.
    Tasks { summaries: Vec<TaskSummary> },
    /// The task is stopped, and this is what has to travel for it to continue.
    Handover { handover: Box<Handover> },
    /// This many raw bytes of the capsule archive follow.
    Capsule { len: u64 },
    /// The request was understood and refused.
    Rejected { reason: String },
}

/// What a client asks a remote to run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRequest {
    pub intent: String,
    /// Which environment, by name. The remote holds the recipe, because that is what
    /// it was sent; it has no need of the client's copy of it.
    pub environment: String,
    pub agent: AgentKind,
    /// Whose work this is, so the remote's record says what the client's would have.
    pub identity_id: String,
    pub identity_label: String,
}

/// A task as a card shows it.
///
/// The whole task does not travel: what a client needs to draw a row is not what a
/// machine needs to run one, and a summary keeps the link's contract to the part the
/// interface actually reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    /// The machine that owns it, which is the machine answering.
    pub host: String,
    pub intent: String,
    pub environment: String,
    pub agent: Option<AgentKind>,
    pub status: TaskExecutionStatus,
    pub mode: TaskExecutionMode,
    /// Something the operator has to look at, in the words the card has room for.
    pub extras: Vec<String>,
}

/// Everything a machine needs to continue a task it did not start.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Handover {
    pub task_id: String,
    /// The task as it was founded: what was asked, in what, by what, and as whom.
    pub draft: HandoverDraft,
    /// Everything that happened to it, so its record continues rather than starts
    /// again.
    pub events: Vec<RecordedEvent>,
    /// The capsule, as an archive this machine can unpack and boot.
    pub capsule: CapsuleArchive,
}

/// One thing that happened, and when it happened.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedEvent {
    pub recorded_at_ms: i64,
    pub event: TaskHistoryEvent,
}

/// The founding facts of a task. These do not travel as events because the machine
/// receiving them writes its own when it founds its record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandoverDraft {
    pub intent: String,
    pub environment: String,
    pub agent: AgentKind,
    /// The conversation to continue, so the agent picks up where it left off.
    pub agent_conversation_id: Option<String>,
}

/// A capsule bundled for travel, and how to tell it arrived whole.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapsuleArchive {
    pub len: u64,
    /// Hex blake3 of the archive, checked where it lands.
    pub digest: String,
}

/// A framed message: a big-endian u32 length, then that many bytes of JSON.
pub(super) async fn write_frame<W, T>(writer: &mut W, value: &T) -> LinkResult<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(format!("frame of {} bytes is too large", body.len()).into());
    }
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    Ok(())
}

pub(super) async fn read_frame<R, T>(reader: &mut R) -> LinkResult<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length = [0u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(format!("frame of {length} bytes is too large").into());
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Reads exactly `len` bytes, refusing anything above `max` before allocating.
pub(super) async fn read_body<R>(reader: &mut R, len: u64, max: u64) -> LinkResult<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    if len > max {
        return Err(format!("{len} bytes exceeds the {max} byte limit").into());
    }
    let mut bytes = vec![0u8; len as usize];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

/// A remote's address, as a string to carry out of band.
///
/// A ticket is the only way to reach a remote: with no relay and no address
/// lookup, a bare public key has nowhere to resolve from. It carries the key and
/// the addresses a remote knows about itself, and it travels between machines in
/// the operator's own hands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTicket {
    addr: EndpointAddr,
    text: String,
}

impl RemoteTicket {
    /// Renders an address as a ticket.
    pub fn new(addr: EndpointAddr) -> LinkResult<Self> {
        let text = serde_json::to_string(&addr)?;
        Ok(Self { addr, text })
    }

    /// The key to dial, without the network addresses.
    pub fn id(&self) -> EndpointId {
        self.addr.id
    }

    /// The port the remote advertises, if it advertises any address at all.
    pub fn port(&self) -> Option<u16> {
        self.addr.ip_addrs().next().map(|socket| socket.port())
    }

    /// A ticket for a remote reached at an address that announces itself.
    ///
    /// This is the one-thing-to-type path: the operator is told where a remote is
    /// and nothing else. The key comes from the remote's own announcement, and a
    /// caller that cares about drift compares it against what it saw last time.
    pub async fn from_broadcast(
        address: &str,
        default_port: u16,
    ) -> LinkResult<(Self, Announcement)> {
        let (host, port) = split_address(address, default_port);
        let announcement = announce::ask(&host, port).await?;
        if announcement.protocol != String::from_utf8_lossy(LINK_ALPN) {
            return Err(format!(
                "{host} speaks {}, not {}",
                announcement.protocol,
                String::from_utf8_lossy(LINK_ALPN)
            )
            .into());
        }
        let key: EndpointId = announcement
            .id
            .parse()
            .map_err(|error| format!("{host} announced an unreadable key: {error}"))?;
        let ticket = Self::from_address(key, &format!("{host}:{port}"), port).await?;
        Ok((ticket, announcement))
    }

    /// A ticket for a remote reached at an address an operator typed.
    ///
    /// The address may be a name or an IP, with or without a port:
    /// `host`, `host:7700`, `10.0.0.4`, `[fd00::1]:7700`. A name is resolved by
    /// the host machine's own resolver, so anything that resolver knows works —
    /// including a Tailscale MagicDNS name, which is the point of asking for a
    /// name rather than an IP.
    ///
    /// The key is not part of the address and is never discovered: the operator
    /// supplies it, and the handshake proves the peer holds it. That is why this
    /// needs no directory and trusts no resolver with anything but addresses.
    pub async fn from_address(
        key: EndpointId,
        address: &str,
        default_port: u16,
    ) -> LinkResult<Self> {
        let (host, port) = split_address(address, default_port);
        let mut addr = EndpointAddr::new(key);
        let mut resolved = 0;
        for socket in lookup_host((host.as_str(), port)).await? {
            addr = addr.with_ip_addr(socket);
            resolved += 1;
        }
        if resolved == 0 {
            return Err(format!("{host} resolved to no addresses").into());
        }
        Self::new(addr)
    }
}

/// Splits `host[:port]` the way a person writes it.
///
/// A bare IPv6 literal is a host with no port, so it is only treated as
/// `host:port` when the tail parses as a port and the head is not itself an
/// address — otherwise `fd00::1` would split at the last colon and lose.
fn split_address(address: &str, default_port: u16) -> (String, u16) {
    let address = address.trim();
    if let Some(rest) = address.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail
            .strip_prefix(':')
            .and_then(|port| port.parse().ok())
            .unwrap_or(default_port);
        return (host.to_owned(), port);
    }
    if address.parse::<Ipv6Addr>().is_ok() {
        return (address.to_owned(), default_port);
    }
    match address.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => match port.parse() {
            Ok(port) => (host.to_owned(), port),
            Err(_) => (address.to_owned(), default_port),
        },
        _ => (address.to_owned(), default_port),
    }
}

impl fmt::Display for RemoteTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FromStr for RemoteTicket {
    type Err = LinkError;

    fn from_str(text: &str) -> LinkResult<Self> {
        let addr: EndpointAddr = serde_json::from_str(text.trim())?;
        Self::new(addr)
    }
}

impl From<RemoteTicket> for EndpointAddr {
    fn from(ticket: RemoteTicket) -> Self {
        ticket.addr
    }
}
