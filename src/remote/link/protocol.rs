use std::{fmt, net::Ipv6Addr, str::FromStr};

use tokio::net::lookup_host;

use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Builder, presets},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::task::{
    LeadingAsk, Task, TaskAsk, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
    TaskHistoryEvent, TaskPlanStep,
};
use crate::tracker::{IssueReference, IssueSnapshot, TrackerKind, TrackerStatus};
use crate::vcs::ContributionState;
use crate::{
    agent::{AgentKind, AgentStanding, AgentWindow, TaskUsage},
    isolation::Pressure,
    project::Project,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::remote::{
    LinkError, LinkResult,
    announce::{self, Announcement},
    bundle::ProfileDigest,
};

pub const LINK_ALPN: &[u8] = b"altitude/link/1";

const MAX_FRAME_BYTES: usize = 64 * 1024;
pub(crate) const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const CHUNK_BYTES: usize = 64 * 1024;
pub(crate) const TRANSFER_PIECE_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) fn builder() -> Builder {
    Endpoint::builder(presets::Minimal)
}

// ------------------------------------------------------------------- the wire

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobHash(String);

impl BlobHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LinkRequest {
    Identify,
    RunTask {
        hash: BlobHash,
        len: u64,
    },
    ListTasks,
    WatchTasks,
    ListSignals {
        task_id: String,
    },
    // A picture is named by the digest of its bytes everywhere, so a machine that
    // reads one in a signal can ask whoever raised it for the bytes.
    ReadPicture {
        id: String,
    },
    Signal {
        command: SignalCommand,
    },
    Input {
        task_id: String,
        body: String,
    },
    HeldProfiles,
    RemoveTask {
        task_id: String,
    },
    TakeTask {
        task_id: String,
    },
    TakeProgress {
        task_id: String,
    },
    FetchTransferPart {
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        len: u64,
    },
    HandedOver {
        transfer_id: String,
    },
    PrepareImport {
        handover: Box<Handover>,
    },
    PutImportPart {
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        hash: BlobHash,
        len: u64,
    },
    AbandonTake {
        transfer_id: String,
    },
    AbandonImport {
        transfer_id: String,
    },
    StageImport {
        transfer_id: String,
    },
    CommitImport {
        transfer_id: String,
    },
    PutConfig {
        hash: BlobHash,
        len: u64,
        take_tracker: bool,
    },
    // A provisioned isolation worker tying itself to this machine. The name is the
    // worker's identity across restarts; the peer key is only where it is now.
    RegisterWorker {
        token: String,
        name: String,
        capacity: WorkerReport,
        // This machine's own ticket reached the host, but the host's ticket never reaches
        // this machine, so the address it will dial is the one said here.
        addr: EndpointAddr,
    },
    WorkerHeartbeat {
        name: String,
    },
    // A host carrying one connection from an agent inside its sandbox to the
    // channel the master is serving for that task.
    AgentChannel {
        socket: String,
        token: String,
    },
}

// What an empty host says it can host. The master keeps its own book of what it
// has promised; this is only the size of the host.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct WorkerReport {
    pub cpus: usize,
    pub memory_mib: u64,
}

#[derive(Clone, Debug)]
pub enum ConfigPush {
    Stored,
    TrackerLinkedElsewhere {
        provider: TrackerKind,
        display_name: String,
    },
}

#[derive(Clone, Debug)]
pub enum WatchEvent {
    Snapshot(Vec<TaskSummary>),
    Changed(Box<TaskSummary>),
    Removed(String),
    Ready,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LinkResponse {
    Identified {
        remote: String,
    },
    Configured,
    TrackerLinkedElsewhere {
        provider: TrackerKind,
        display_name: String,
    },
    Accepted,
    Tasks {
        summaries: Vec<TaskSummary>,
    },
    TaskChanged {
        summary: Box<TaskSummary>,
    },
    TaskRemoved {
        id: String,
    },
    Ready,
    Signals {
        signals: Vec<TaskAsk>,
    },
    Picture {
        id: String,
        len: u64,
    },
    HeldProfiles {
        profiles: Vec<ProfileDigest>,
    },
    Removed,
    Handover {
        handover: Box<Handover>,
    },
    ImportPrepared,
    Imported {
        task_id: String,
    },
    TransferPiece {
        len: u64,
    },
    Collecting {
        stage: String,
        done: u64,
        total: u64,
        unit: String,
    },
    Rejected {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRequest {
    pub origin: TaskOrigin,
    pub project_id: String,
    pub agent: AgentKind,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub host: String,
    pub title: String,
    #[serde(default)]
    pub title_generated: bool,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub shown_updated_at_ms: i64,
    pub project: String,
    #[serde(default)]
    pub project_id: Option<String>,
    pub agent: Option<AgentKind>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub plan: Vec<TaskPlanStep>,
    pub status: TaskExecutionStatus,
    pub mode: TaskExecutionMode,
    #[serde(default)]
    pub pause_reason: Option<TaskExecutionPauseReason>,
    // When a limit on the account clears, and which of its windows ran out: the one
    // pause that ends without anyone being asked.
    #[serde(default)]
    pub resumes_at_ms: Option<i64>,
    #[serde(default)]
    pub limit_window: Option<AgentWindow>,
    // What the task has spent, carried so a machine holding someone else's task can
    // say the same thing about it as the machine that ran it.
    #[serde(default)]
    pub usage: TaskUsage,
    // The account the task's credential runs on, as of the last run that looked.
    #[serde(default)]
    pub standing: Option<AgentStanding>,
    // What the agent is doing right now. Live only: it is replaced by the next
    // line, and a summary that has none describes a task that is not working.
    #[serde(default)]
    pub activity: Option<String>,
    // The ask a task is in the middle of, with the thread's own answer to whether it
    // is waiting: a row can say what the agent asked and whether it needs them.
    #[serde(default)]
    pub leading_ask: Option<LeadingAsk>,
    // Where the task stands on the machine holding it, for a task waiting for room:
    // the numbering is the machine's, so a client cannot work it out.
    #[serde(default)]
    pub ahead: Option<usize>,
    #[serde(default)]
    pub holding: Option<Pressure>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub tracker_issue: Option<IssueReference>,
    #[serde(default)]
    pub tracker_state: Option<TrackerStatus>,
    #[serde(default)]
    pub operator_label: Option<String>,
    #[serde(default)]
    pub contributions: Vec<ContributionSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContributionSummary {
    pub reference: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub state: Option<ContributionState>,
}

impl TaskSummary {
    pub fn of(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            host: task.host.clone(),
            title: task.title().to_owned(),
            title_generated: task.facts.title.is_none(),
            created_at_ms: task.facts.created_at_ms,
            shown_updated_at_ms: task.facts.shown_updated_at_ms,
            project: task
                .facts
                .project
                .as_ref()
                .map(|project| project.name.clone())
                .unwrap_or_default(),
            project_id: task
                .facts
                .project
                .as_ref()
                .map(|project| project.id.clone()),
            agent: task.agent_kind,
            model: task.facts.model.clone(),
            plan: task.facts.plan.clone().unwrap_or_default(),
            status: task.facts.execution_status.clone(),
            mode: task.facts.execution_mode,
            pause_reason: task.facts.execution_pause_reason.clone(),
            resumes_at_ms: task.facts.resumes_at_ms,
            limit_window: task.facts.limit_window.clone(),
            usage: task.facts.usage.clone(),
            standing: task.facts.standing.clone(),
            activity: None,
            leading_ask: task.facts.leading_ask.clone(),
            ahead: None,
            holding: None,
            last_error: task.facts.last_error.clone(),
            tracker_issue: task.facts.tracker_issue.clone(),
            tracker_state: task.facts.tracker_status,
            operator_label: task.facts.operator_label.clone(),
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
                        url: contribution.url.clone(),
                        summary: observed.map(|observation| observation.summary.clone()),
                        state: observed.map(|observation| observation.state.clone()),
                    }
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handover {
    pub task_id: String,
    pub transfer_id: String,
    pub history: TransferArchive,
    pub work: TransferArchive,
    pub transcript: TransferArchive,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum TransferPart {
    History,
    Work,
    Transcript,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandoverRecord {
    pub draft: HandoverDraft,
    pub events: Vec<RecordedEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedEvent {
    pub recorded_at_ms: i64,
    pub event: TaskHistoryEvent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandoverDraft {
    pub intent: String,
    pub project: Project,
    pub agent: AgentKind,
    pub agent_conversation_id: Option<String>,
}

// A capsule is a directory of plain files whose names both machines agree on. The
// work archive travels compressed because it crosses a network, and is expanded on
// arrival because the side that puts a tree into a sandbox reads it by offset.
pub const HISTORY_FILE: &str = "history.json";
pub const WORK_FILE: &str = "work.tar.gz";
// The expanded archive: what the isolation layer writes and reads by offset, and
// what is filed under a task's id until provisioning puts it in a sandbox.
pub const WORK_PLAIN_FILE: &str = "work.tar";
pub const TRANSCRIPT_FILE: &str = "transcript.jsonl";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferArchive {
    pub len: u64,
    pub digest: String,
}

pub(crate) async fn write_frame<W, T>(writer: &mut W, value: &T) -> LinkResult<()>
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

pub(crate) async fn read_frame<R, T>(reader: &mut R) -> LinkResult<T>
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

pub(crate) async fn read_body<R>(reader: &mut R, len: u64, max: u64) -> LinkResult<Vec<u8>>
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTicket {
    addr: EndpointAddr,
    text: String,
}

impl RemoteTicket {
    pub fn new(addr: EndpointAddr) -> LinkResult<Self> {
        let text = serde_json::to_string(&addr)?;
        Ok(Self { addr, text })
    }

    pub fn id(&self) -> EndpointId {
        self.addr.id
    }

    pub fn port(&self) -> Option<u16> {
        self.addr.ip_addrs().next().map(|socket| socket.port())
    }

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
