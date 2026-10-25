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

use crate::task::{
    Task, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent,
    TaskOperatorAttentionLevel, TaskOperatorSignalKind, TaskOperatorSignalThread,
};
use crate::tracker::{IssueReference, IssueSnapshot, TrackerKind};
use crate::vcs::ContributionState;
use crate::{agent::AgentKind, isolation::Environment};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{
    LinkError, LinkResult,
    announce::{self, Announcement},
    bundle::ProfileDigest,
};

/// The one ALPN this protocol speaks. Two endpoints that do not agree on this
/// string never complete a handshake.
pub const LINK_ALPN: &[u8] = b"altitude/link/1";

/// Upper bound on a framed message. Requests are small; blob bytes travel
/// outside the frame.
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Upper bound on a single blob, so a peer cannot make a remote allocate
/// without limit. Disk archives travel as bounded pieces instead.
pub(super) const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
/// Write granularity for blob bytes.
pub(super) const CHUNK_BYTES: usize = 64 * 1024;
pub(super) const TRANSFER_PIECE_BYTES: u64 = 4 * 1024 * 1024;

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
    RunTask {
        hash: BlobHash,
        len: u64,
    },
    /// Ask what tasks the remote owns, once.
    ListTasks,
    /// Watch what the remote owns: the answer is what it holds now, followed by
    /// one frame per change for as long as this stream is open.
    ///
    /// A list is a question with a moment for an answer, and a card list is not
    /// that: what it shows is a machine's state, which changes when it changes.
    WatchTasks,
    ListSignals {
        task_id: String,
    },
    Signal {
        command: SignalCommand,
    },
    /// Ask what profiles the remote already holds for the asking peer.
    ///
    /// Authenticated: the answer is the asking peer's own held stamps, not
    /// whatever the remote holds for everyone, which is why it is a request
    /// rather than something an anonymous announcement carries.
    HeldProfiles,
    /// Remove a task and dispose of its capsule.
    ///
    /// A remote is compute, not a different kind of task: deleting one of its
    /// tasks is the same act as deleting one held here, performed by the machine
    /// that holds it.
    RemoveTask {
        task_id: String,
    },
    /// Ask the remote to hand a task over.
    ///
    /// Preparation stops the task but keeps ownership at the source until the
    /// recipient has durably staged its history and booted the capsule.
    TakeTask {
        task_id: String,
    },
    FetchTransferPart {
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        len: u64,
    },
    /// The prepared recipient has staged and verified the task. Committing the
    /// source removal is idempotent so a lost acknowledgement cannot lose ownership.
    HandedOver {
        transfer_id: String,
    },
    /// Offer a stopped task to a machine that will continue it.
    ///
    /// This is the push side of a transfer: the source dials the recipient, so
    /// nothing has to reach back into the source's network. The answer says
    /// whether the environment archive still has to travel.
    PrepareImport {
        handover: Box<Handover>,
    },
    /// Announce one bounded piece of an archive: `len` raw bytes follow, whose
    /// digest must be `hash`, and they belong at `offset`.
    PutImportPart {
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        hash: BlobHash,
        len: u64,
    },
    /// Give up a prepared transfer, putting the task back the way it was.
    ///
    /// A stopped transfer is not a lost task: whoever was taking it says so, and
    /// the machine that owns it runs it again rather than holding it stopped
    /// forever.
    AbandonTake {
        transfer_id: String,
    },
    /// Give up a staged import, so a stopped push leaves nothing half-written.
    AbandonImport {
        transfer_id: String,
    },
    /// The recipient has every piece and may land and boot the task.
    StageImport {
        transfer_id: String,
    },
    /// The source has relinquished ownership. Begin the landed task.
    CommitImport {
        transfer_id: String,
    },
    /// Announce an upload: `len` raw bytes follow, whose digest must be `hash`.
    PutBlob {
        hash: BlobHash,
        len: u64,
    },
    /// Announce a configuration: `len` raw bytes follow, whose digest must be
    /// `hash`. It is what this client runs tasks with, so a remote that holds it
    /// can build the same environments.
    ///
    /// `take_tracker` says whether a task tracker account that is already linked
    /// for another client may be taken over. It is false until someone has been
    /// asked: an account is who the work is attributed to, and a machine that
    /// quietly reassigned one would be attributing somebody else's work.
    PutConfig {
        hash: BlobHash,
        len: u64,
        take_tracker: bool,
    },
}

/// What a remote did with a pushed configuration.
#[derive(Clone, Debug)]
pub enum ConfigPush {
    /// It holds the configuration now.
    Stored,
    /// The tracker account already belongs to another client there, and nothing
    /// was written until somebody says which client it should be.
    TrackerLinkedElsewhere {
        provider: TrackerKind,
        display_name: String,
    },
}

/// One thing a watch reported: what the machine holds, what changed, and when it
/// became able to take work.
///
/// The snapshot is what it holds when the watch opens; the changes are what has
/// happened since. A change that repeats a task already in the snapshot costs
/// nothing, because a card is keyed by its task.
#[derive(Clone, Debug)]
pub enum WatchEvent {
    Snapshot(Vec<TaskSummary>),
    Changed(Box<TaskSummary>),
    Removed(String),
    Ready,
}

/// The remote's answer to a [`LinkRequest`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LinkResponse {
    /// Who the remote is.
    Identified {
        remote: String,
    },
    /// The upload arrived and its digest matched.
    Stored {
        hash: BlobHash,
        len: u64,
    },
    /// The configuration arrived, and the remote holds it now.
    Configured,
    /// The tracker account this configuration carries is already linked for
    /// another client, so nothing was written.
    TrackerLinkedElsewhere {
        provider: TrackerKind,
        /// The account as it names itself, which came from the configuration being
        /// pushed rather than from the client that already had it.
        display_name: String,
    },
    /// The remote took the task. What it called it is read from the machine rather
    /// than returned, because the machine is where the task is.
    Accepted,
    /// What the remote owns, as its own cards would show it.
    Tasks {
        summaries: Vec<TaskSummary>,
    },
    /// One task changed, sent while a watch is open.
    TaskChanged {
        summary: Box<TaskSummary>,
    },
    /// One task is gone, sent while a watch is open.
    ///
    /// Sent to every watcher: a task id a peer does not know is not one of its
    /// tasks, and the id says nothing about whose it was.
    TaskRemoved {
        id: String,
    },
    /// The machine can be given work now, which is not a change to any task.
    Ready,
    Signals {
        signals: Vec<TaskOperatorSignalThread>,
    },
    /// What the remote holds for the asking peer's profiles.
    HeldProfiles {
        profiles: Vec<ProfileDigest>,
    },
    /// The remote removed the task and disposed of its capsule.
    Removed,
    /// The task is stopped, and this is what has to travel for it to continue.
    Handover {
        handover: Box<Handover>,
    },
    /// The recipient accepted the transfer and this is whether it needs the
    /// environment archive.
    ImportPrepared {
        needs_environment: bool,
    },
    /// The recipient landed and booted the task, and this is what it is called here.
    Imported {
        task_id: String,
    },
    TransferPiece {
        len: u64,
    },
    /// The request was understood and refused.
    Rejected {
        reason: String,
    },
}

/// What a client asks a remote to run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRequest {
    pub origin: TaskOrigin,
    /// Which environment, by name. The remote holds the recipe, because that is what
    /// it was sent; it has no need of the client's copy of it.
    pub environment: String,
    pub agent: AgentKind,
    /// How the requesting operator reads to a human, so the remote's record says
    /// what the client's would have. Presentation only: whose work this is comes
    /// from the authenticated peer, not from here.
    pub identity_label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskOrigin {
    Intent(String),
    Issue(Box<IssueSnapshot>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SignalCommand {
    Reply {
        task_id: String,
        signal_id: String,
        body: String,
    },
    Resolve {
        task_id: String,
        signal_id: String,
        resolved: bool,
    },
    Accept {
        task_id: String,
        signal_id: String,
    },
}

/// A task as a card shows it.
///
/// The whole task does not travel: what a client needs to draw a row is not what a
/// machine needs to run one, and a summary keeps the link's contract to the part the
/// interface actually reads. This is also what a card is drawn from whichever
/// machine owns the task, so a task here and a task on a remote read the same way.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    /// The machine that owns it, which is the machine answering.
    pub host: String,
    /// What a card calls it: the agent's name for the work, or its first line.
    ///
    /// Resolved by the machine that owns the task, because that is where the
    /// intent is and a card is not where a sentence gets cut.
    pub title: String,
    pub environment: String,
    pub agent: Option<AgentKind>,
    pub status: TaskExecutionStatus,
    pub mode: TaskExecutionMode,
    /// Why it is not running, when it is not.
    #[serde(default)]
    pub pause_reason: Option<TaskExecutionPauseReason>,
    /// How loudly this task is asking to be looked at.
    pub attention: TaskOperatorAttentionLevel,
    /// The blocking signal the operator is being waited on for, when one is open.
    #[serde(default)]
    pub blocking_signal_kind: Option<TaskOperatorSignalKind>,
    #[serde(default)]
    pub unresolved_signal_count: u32,
    /// What went wrong, when something did.
    #[serde(default)]
    pub last_error: Option<String>,
    /// The tracker issue this task is bound to, when it is bound to one, so a
    /// client can act on the binding without reading the whole task.
    #[serde(default)]
    pub tracker_issue: Option<IssueReference>,
    /// How the operator whose work this is reads to a human.
    #[serde(default)]
    pub operator_label: Option<String>,
    /// What the agent last reported, in the words a card has room for.
    #[serde(default)]
    pub latest_turn_summary: Option<String>,
    /// What this task has open for review, so a client reads the same rows the
    /// owning machine would.
    #[serde(default)]
    pub contributions: Vec<ContributionSummary>,
}

/// One pull request a task has open, as the owning machine last read it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContributionSummary {
    pub reference: String,
    /// One operator-facing line, e.g. `ci 1 failing, 2 passing · merge blocked`.
    #[serde(default)]
    pub summary: Option<String>,
    /// What the provider last reported. Its meaning is read where it is drawn,
    /// because a contribution settles with time and the clock that reads it is
    /// the reader's.
    #[serde(default)]
    pub state: Option<ContributionState>,
}

impl TaskSummary {
    /// The card a task is drawn as, on the machine that owns it.
    ///
    /// One projection for both machines: a remote fills its answer with this, and
    /// the client draws its own tasks with the same call, so the two cannot drift
    /// into saying different things about the same kind of task.
    pub fn of(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            host: task.host.clone(),
            title: task.title().to_owned(),
            environment: task.facts.environment_name.clone(),
            agent: task.agent_kind,
            status: task.facts.execution_status.clone(),
            mode: task.facts.execution_mode,
            pause_reason: task.facts.execution_pause_reason.clone(),
            attention: task.facts.operator_attention_level.clone(),
            blocking_signal_kind: task.facts.blocking_signal_kind,
            unresolved_signal_count: task.facts.unresolved_signal_count,
            last_error: task.facts.last_error.clone(),
            tracker_issue: task.facts.tracker_issue.clone(),
            operator_label: task.facts.operator_label.clone(),
            latest_turn_summary: task.facts.latest_turn_summary.clone(),
            contributions: task
                .code_contributions
                .iter()
                .map(|contribution| {
                    let observed = task
                        .code_contribution_observations
                        .iter()
                        .find(|observation| observation.contribution_id == contribution.id);
                    ContributionSummary {
                        reference: contribution.reference.clone(),
                        summary: observed.map(|observation| observation.summary.clone()),
                        state: observed.map(|observation| observation.state.clone()),
                    }
                })
                .collect(),
        }
    }
}

/// Everything a machine needs to continue a task it did not start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handover {
    pub task_id: String,
    pub transfer_id: String,
    pub environment: Option<EnvironmentArchive>,
    pub history: TransferArchive,
    pub capsule: TransferArchive,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum TransferPart {
    Environment,
    History,
    Capsule,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentArchive {
    pub snapshot_id: String,
    pub descriptor_digest: String,
    pub layer_roots: Vec<String>,
    pub archive: TransferArchive,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandoverRecord {
    pub draft: HandoverDraft,
    pub events: Vec<RecordedEvent>,
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
    pub environment: Environment,
    pub agent: AgentKind,
    /// The conversation to continue, so the agent picks up where it left off.
    pub agent_conversation_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferArchive {
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
