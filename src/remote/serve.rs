//! The serve role: work for clients, with no terminal.
//!
//! A different execution mode, not a different configuration of the same one. A
//! serve role has nobody at the keyboard, so it never takes the terminal, it logs to
//! stdout for whatever service manager started it, and it reads its own settings
//! file rather than the client's.

use std::{
    collections::HashMap,
    error::Error,
    fs, io,
    io::{Read, Seek, SeekFrom},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use crate::{
    isolation::capsule::{ENVIRONMENT_LABEL, EnvironmentBase},
    task::TaskExecutionPauseReason,
    vcs::now_ms,
};
use iroh::EndpointId;
use microsandbox::{SaveOpts, Snapshot};
use tokio::{
    sync::{Mutex as AsyncMutex, Semaphore},
    task::spawn_blocking,
};

use super::send::IncomingTransfers;
use super::{Handover, HandoverDraft, RecordedEvent, TransferArchive, WatchEvent};

use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::{mpsc, watch},
    time::{MissedTickBehavior, interval},
};

use crate::{
    agent::AgentEngines,
    cli::{ServeArgs, ServeCommand, Tracker},
    config::{Config, DEFAULT_PORT, InstallationIdentity, LinearSettings, ServeSettings},
    db::Database,
    isolation::{self, IsolationEngine, IsolationStatus, MicrosandboxIsolation},
    logging,
    remote::protocol::{TaskRequest, TaskSummary},
    remote::{
        ClientRecord, InstallationKey, LinkServer, TrackerConnection,
        announce::{RemoteState, TrackerReadiness},
        server::{BoxFuture, ServerParts, TaskHost},
    },
    task::{Task, TaskEventLoop, TaskExecutionUpdate, TaskService, TaskStore},
    task::{TaskHistoryEvent, TaskOperatorSignalThread, TrackerHost},
    tracker::{
        IssueReader, IssueSnapshot, Linear, TrackerKind, TrackerProvider,
        automation::LinearAutomation, oauth,
    },
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PreparedTransfer {
    recipient: String,
    handover: Handover,
}

/// How long a prepared transfer may sit still before the task is taken back.
///
/// Long enough that a slow link is not mistaken for an abandoned one, because
/// what arrives is the only sign that a recipient is still there.
const TRANSFER_IDLE_MS: i64 = 120_000;

struct OwnTasks {
    config: Arc<Config>,
    tasks: Arc<TaskEventLoop<MicrosandboxIsolation>>,
    incoming: IncomingTransfers,
    preparation_lock: Arc<AsyncMutex<()>>,
    /// When each prepared transfer was last read from, by its id. A transfer is
    /// alive while pieces are being fetched; the history says when it was staged,
    /// which is the floor a restart starts from.
    activity: Arc<Mutex<HashMap<String, i64>>>,
    /// Who is watching this machine's tasks, and where each one's changes go.
    watchers: Arc<Watchers>,
    read_slots: Arc<Semaphore>,
    readiness: watch::Sender<RemoteState>,
    runtime: watch::Receiver<IsolationStatus>,
    tracker: Option<Arc<super::tracker::RemoteTracker>>,
    /// Whether this machine may start work that arrives from a tracker, whatever
    /// the project says about itself.
    accept_delegated_work: bool,
}

/// Who is watching this machine's tasks, one entry per open watch.
///
/// A watch is a peer that asked what this machine holds and is still listening;
/// what it may see is its own work, which is the whole reason the filter is here
/// and not in the task list.
#[derive(Default)]
struct Watchers {
    peers: Mutex<Vec<WatchingPeer>>,
}

struct WatchingPeer {
    peer: EndpointId,
    changes: mpsc::UnboundedSender<WatchEvent>,
}

impl Watchers {
    fn subscribe(&self, peer: EndpointId) -> mpsc::UnboundedReceiver<WatchEvent> {
        let (changes, receiving) = mpsc::unbounded_channel();
        let mut peers = self.peers.lock().expect("watchers lock poisoned");
        peers.retain(|watching| !watching.changes.is_closed());
        peers.push(WatchingPeer { peer, changes });
        receiving
    }

    /// A task changed, and this is who it belongs to.
    fn changed(&self, operator: &str, summary: TaskSummary) {
        let mut peers = self.peers.lock().expect("watchers lock poisoned");
        peers.retain(|watching| {
            if watching.changes.is_closed() {
                return false;
            }
            if watching.peer.to_string() == operator {
                let _ = watching
                    .changes
                    .send(WatchEvent::Changed(Box::new(summary.clone())));
            }
            true
        });
    }

    /// The machine can be given work, which nothing about a task says.
    fn ready(&self) {
        let mut peers = self.peers.lock().expect("watchers lock poisoned");
        peers.retain(|watching| {
            if watching.changes.is_closed() {
                return false;
            }
            let _ = watching.changes.send(WatchEvent::Ready);
            true
        });
    }

    /// A task is gone. Every watcher is told: an id that is not one of a peer's
    /// tasks changes nothing there, and the id says nothing about whose it was.
    fn removed(&self, id: String) {
        let mut peers = self.peers.lock().expect("watchers lock poisoned");
        peers.retain(|watching| {
            if watching.changes.is_closed() {
                return false;
            }
            let _ = watching.changes.send(WatchEvent::Removed(id.clone()));
            true
        });
    }

    fn publish(&self, task: &Task) {
        if let Some(operator) = task.operator_id() {
            self.changed(operator, TaskSummary::of(task));
        }
    }
}

impl OwnTasks {
    /// Take back tasks whose transfer stopped moving.
    ///
    /// A recipient that goes away mid-transfer leaves the task stopped here,
    /// owned and run by nobody. Nothing on the other side will ever say so, so the
    /// machine that owns the task decides: after a while with no piece read, the
    /// task waits to run here again.
    async fn release_stalled_transfers(&self) {
        // Packaging holds this lock, and a transfer being packaged is not stalled.
        let Ok(_packaging) = self.preparation_lock.try_lock() else {
            return;
        };
        let tasks = match self.tasks.list().await {
            Ok(tasks) => tasks,
            Err(error) => {
                tracing::warn!(%error, "could not read tasks to release a stalled transfer");
                return;
            }
        };
        for task in tasks {
            if task.facts.execution_pause_reason != Some(TaskExecutionPauseReason::Transfer) {
                continue;
            }
            let Some((transfer_id, staged_at)) = self.staged_transfer(&task.id).await else {
                continue;
            };
            let last = self
                .activity
                .lock()
                .expect("transfer activity lock poisoned")
                .get(&transfer_id)
                .copied()
                .unwrap_or(staged_at);
            if now_ms() - last < TRANSFER_IDLE_MS {
                continue;
            }
            if let Err(error) = self.tasks.abandon_transfer(&task.id, &transfer_id).await {
                tracing::warn!(task = %task.id, %transfer_id, %error, "could not take a stalled transfer back");
                continue;
            }
            tracing::info!(task = %task.id, %transfer_id, "a transfer stopped moving; the task stays here");
            self.activity
                .lock()
                .expect("transfer activity lock poisoned")
                .remove(&transfer_id);
            if let Ok(directory) = self.transfer_directory(&transfer_id) {
                for name in ["environment.tar.zst", "capsule.tar.zst", "history.json"] {
                    let _ = fs::remove_file(directory.join(name));
                }
            }
        }
    }

    /// The transfer a task is staged for, and when it was staged.
    async fn staged_transfer(&self, task_id: &str) -> Option<(String, i64)> {
        self.tasks
            .events(task_id)
            .await
            .ok()?
            .into_iter()
            .rev()
            .find_map(|(recorded_at_ms, event)| match event {
                TaskHistoryEvent::TransferStaged { transfer_id, .. } => {
                    Some((transfer_id, recorded_at_ms))
                }
                _ => None,
            })
    }

    fn transfer_directory(&self, id: &str) -> Result<PathBuf, String> {
        let id = uuid::Uuid::parse_str(id).map_err(|_| "invalid transfer identifier".to_owned())?;
        Ok(self
            .config
            .data_dir()
            .join("transfers")
            .join(id.to_string()))
    }
}

impl TaskHost for OwnTasks {
    fn signal_threads(
        &self,
        peer: EndpointId,
        task_id: String,
    ) -> BoxFuture<Result<Vec<TaskOperatorSignalThread>, String>> {
        let tasks = self.tasks.clone();
        Box::pin(async move {
            let task = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| {
                    task.id == task_id && task.operator_id() == Some(peer.to_string().as_str())
                })
                .ok_or("this task is not active under your identity")?;
            tasks
                .service()
                .signal_threads(&task.id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn signal(
        &self,
        peer: EndpointId,
        command: super::protocol::SignalCommand,
    ) -> BoxFuture<Result<(), String>> {
        let tasks = self.tasks.clone();
        Box::pin(async move {
            use super::protocol::SignalCommand;
            let task = match &command {
                SignalCommand::Reply {
                    task_id, signal_id, ..
                }
                | SignalCommand::Resolve {
                    task_id, signal_id, ..
                }
                | SignalCommand::Accept { task_id, signal_id } => tasks
                    .service()
                    .task_for_signal(task_id, signal_id)
                    .await
                    .map_err(|error| error.to_string())?,
            }
            .ok_or("this task is no longer active")?;
            if task.operator_id() != Some(peer.to_string().as_str()) {
                return Err("this task belongs to another operator".into());
            }
            if !tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .iter()
                .any(|active| active.id == task.id)
            {
                return Err("this task is no longer active".into());
            }
            match command {
                SignalCommand::Reply {
                    task_id,
                    signal_id,
                    body,
                } => tasks
                    .reply(&task_id, &signal_id, body)
                    .await
                    .map_err(|error| error.to_string())?,
                SignalCommand::Resolve {
                    task_id,
                    signal_id,
                    resolved,
                } => {
                    tasks
                        .service()
                        .resolve_signal(&task_id, &signal_id, resolved)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                SignalCommand::Accept { task_id, signal_id } => {
                    tasks
                        .service()
                        .confirm_operator_signal(&task_id, &signal_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        })
    }

    fn hand_over(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<Handover, String>> {
        let tasks = self.tasks.clone();
        let config = self.config.clone();
        let lock = self.preparation_lock.clone();
        let activity = Arc::clone(&self.activity);
        Box::pin(async move {
            let _preparing = lock.lock().await;
            if let Some(task) = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| task.id == task_id)
                && task.operator_id() != Some(peer.to_string().as_str())
            {
                return Err("that task belongs to another operator".into());
            }
            let handover = export_task(&config, &tasks, &task_id, &peer.to_string()).await?;
            // Packaging is done, so the wait for the first piece starts now rather
            // than from when the task was staged.
            activity
                .lock()
                .expect("transfer activity lock poisoned")
                .insert(handover.transfer_id.clone(), now_ms());
            Ok(handover)
        })
    }

    fn transfer_piece(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: super::TransferPart,
        offset: u64,
        len: u64,
    ) -> BoxFuture<Result<Vec<u8>, String>> {
        let directory = self.transfer_directory(&transfer_id);
        let read_slots = Arc::clone(&self.read_slots);
        let activity = Arc::clone(&self.activity);
        Box::pin(async move {
            if len == 0 || len > super::protocol::TRANSFER_PIECE_BYTES {
                return Err("invalid transfer piece length".into());
            }
            let _permit = read_slots
                .acquire_owned()
                .await
                .map_err(|e| e.to_string())?;
            let directory = directory?;
            let prepared: PreparedTransfer = serde_json::from_slice(
                &fs::read(directory.join("transfer.json")).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            if prepared.recipient != peer.to_string() {
                return Err("this transfer belongs to another recipient".into());
            }
            activity
                .lock()
                .expect("transfer activity lock poisoned")
                .insert(transfer_id.clone(), now_ms());
            let filename = match part {
                super::TransferPart::Environment => "environment.tar.zst",
                super::TransferPart::History => "history.json",
                super::TransferPart::Capsule => "capsule.tar.zst",
            };
            let path = directory.join(filename);
            spawn_blocking(move || {
                let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
                let size = file.metadata().map_err(|e| e.to_string())?.len();
                if offset.checked_add(len).is_none_or(|end| end > size) {
                    return Err("transfer piece is outside the archive".into());
                }
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| e.to_string())?;
                let mut bytes = vec![0; len as usize];
                file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                Ok(bytes)
            })
            .await
            .map_err(|e| e.to_string())?
        })
    }

    fn prepare_import(
        &self,
        peer: EndpointId,
        handover: Handover,
    ) -> BoxFuture<Result<bool, String>> {
        let incoming = self.incoming.clone();
        Box::pin(async move { incoming.prepare(peer, handover).await })
    }

    fn put_import_part(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: super::TransferPart,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<Result<(), String>> {
        let incoming = self.incoming.clone();
        Box::pin(async move { incoming.put(peer, &transfer_id, part, offset, &bytes).await })
    }

    fn abandon_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<(), String>> {
        let incoming = self.incoming.clone();
        Box::pin(async move { incoming.abandon(peer, &transfer_id).await })
    }

    fn abandon_take(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>> {
        let tasks = Arc::clone(&self.tasks);
        let root = self.config.data_dir().join("transfers");
        Box::pin(async move {
            let mut staged = None;
            for task in tasks.list().await.map_err(|error| error.to_string())? {
                let history = tasks
                    .events(&task.id)
                    .await
                    .map_err(|error| error.to_string())?;
                let matches = history.iter().rev().find_map(|(_, event)| match event {
                    TaskHistoryEvent::TransferStaged {
                        transfer_id: current,
                        peer: recipient,
                        ..
                    } => Some(current == &transfer_id && recipient == &peer.to_string()),
                    _ => None,
                });
                if matches == Some(true) {
                    staged = Some(task);
                    break;
                }
            }
            let task = staged.ok_or("that transfer is no longer staged")?;
            tasks
                .abandon_transfer(&task.id, &transfer_id)
                .await
                .map_err(|error| error.to_string())?;
            for name in ["environment.tar.zst", "capsule.tar.zst", "history.json"] {
                let _ = fs::remove_file(root.join(&transfer_id).join(name));
            }
            Ok(())
        })
    }

    fn stage_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<String, String>> {
        let incoming = self.incoming.clone();
        let tasks = Arc::clone(&self.tasks);
        let tracker = self.tracker.clone();
        Box::pin(async move {
            let task_id = incoming.stage(peer, &transfer_id).await?;
            // A task that arrives already bound to an issue is reported here now,
            // through a session on that issue, without replaying its history.
            if let Some(tracker) = tracker
                && tracker.has_account(peer)
                && let Some(task) = tasks
                    .list()
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .find(|task| task.id == task_id)
                && let Some(reference) = task.facts.tracker_issue.clone()
                && let Err(error) = tracker.pilot_task(peer, task_id.clone(), reference).await
            {
                tracing::warn!(task = %task_id, %error, "could not pilot a transferred bound task");
            }
            Ok(task_id)
        })
    }

    fn commit_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<(), String>> {
        let incoming = self.incoming.clone();
        Box::pin(async move { incoming.commit(peer, &transfer_id).await })
    }

    fn handed_over(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>> {
        let directory = self.transfer_directory(&transfer_id);
        let tasks = Arc::clone(&self.tasks);
        let preparation_lock = Arc::clone(&self.preparation_lock);
        Box::pin(async move {
            let _preparing = preparation_lock.lock().await;
            let directory = directory?;
            let prepared: PreparedTransfer = serde_json::from_slice(
                &fs::read(directory.join("transfer.json")).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            if prepared.recipient != peer.to_string() {
                return Err("this transfer belongs to another recipient".into());
            }
            if transfer_committed(&tasks, &prepared).await? {
                return Ok(());
            }
            let task = tasks
                .list()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|task| task.id == prepared.handover.task_id)
                .ok_or_else(|| "the source task is missing".to_owned())?;
            if task.facts.execution_pause_reason != Some(TaskExecutionPauseReason::Transfer) {
                return Err("the source task is no longer staged for transfer".into());
            }
            let matches_preparation = tasks
                .events(&task.id)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .rev()
                .find_map(|(_, event)| match event {
                    TaskHistoryEvent::TransferStaged {
                        transfer_id: current,
                        peer: recipient,
                        ..
                    } => Some(current == transfer_id && recipient == peer.to_string()),
                    _ => None,
                })
                .unwrap_or(false);
            if !matches_preparation {
                return Err("the task belongs to another transfer preparation".into());
            }
            tasks
                .relinquish(task, &transfer_id, &peer.to_string())
                .await
                .map_err(|e| e.to_string())?;
            for name in ["environment.tar.zst", "capsule.tar.zst", "history.json"] {
                let _ = fs::remove_file(directory.join(name));
            }
            Ok(())
        })
    }

    fn configure(&self, peer: EndpointId) -> BoxFuture<Result<(), String>> {
        let config = Arc::clone(&self.config);
        let tasks = Arc::clone(&self.tasks);
        let mut runtime = self.runtime.clone();
        Box::pin(async move {
            wait_until_ready(&mut runtime).await?;
            let owner = peer.to_string();
            let settings = config
                .for_operator(&owner)
                .map_err(|e| e.to_string())?
                .load_or_create_settings()
                .map_err(|e| e.to_string())?;
            let agent = settings.settings().agent().unwrap_or_default();
            for project in settings.settings().projects() {
                tasks
                    .prepare_project_as_operator(&owner, project, agent)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
    }

    fn run(&self, peer: EndpointId, request: TaskRequest) -> BoxFuture<Result<(), String>> {
        let config = Arc::clone(&self.config);
        let tasks = Arc::clone(&self.tasks);
        let tracker = self.tracker.clone();
        Box::pin(async move {
            let owner = peer.to_string();
            let settings = config
                .for_operator(&owner)
                .map_err(|e| e.to_string())?
                .load_or_create_settings()
                .map_err(|e| e.to_string())?;
            let project = settings
                .settings()
                .project(&request.project_id)
                .cloned()
                .ok_or_else(|| format!("this machine has no project {}", request.project_id))?;
            tasks
                .prepare_project_as_operator(&owner, &project, request.agent)
                .await
                .map_err(|e| e.to_string())?;
            let operator = InstallationIdentity {
                id: owner,
                label: request.identity_label,
            };
            match request.origin {
                super::protocol::TaskOrigin::Intent(intent) => {
                    tasks
                        .service()
                        .create(intent, project, request.agent, Some(operator))
                        .await
                        .map_err(|error| error.to_string())?;
                    tasks.admit();
                }
                super::protocol::TaskOrigin::Issue(issue) => {
                    let task = tasks
                        .create_issue_for_operator(*issue, project, request.agent, operator)
                        .await
                        .map_err(|error| error.to_string())?;
                    // A session can only be piloted for a peer that has synced a
                    // tracker account; without one the work still runs, it just
                    // cannot be reported to the tracker from here.
                    if let Some(tracker) = &tracker
                        && tracker.has_account(peer)
                        && let Some(reference) = task.facts.tracker_issue.clone()
                        && let Err(error) =
                            tracker.pilot_task(peer, task.id.clone(), reference).await
                    {
                        tracing::warn!(task = %task.id, %error, "could not pilot a delegated issue task");
                    }
                }
            }
            Ok(())
        })
    }

    fn list(&self, peer: EndpointId) -> BoxFuture<Result<Vec<TaskSummary>, String>> {
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            let owner = peer.to_string();
            Ok(tasks
                .list()
                .await
                .map_err(|e| e.to_string())?
                .iter()
                .filter(|task| task.operator_id() == Some(owner.as_str()))
                .map(TaskSummary::of)
                .collect())
        })
    }

    fn watch(&self, peer: EndpointId) -> BoxFuture<Result<super::server::Watching, String>> {
        let tasks = Arc::clone(&self.tasks);
        let watchers = Arc::clone(&self.watchers);
        Box::pin(async move {
            // Registered before the snapshot is read, so a change made while it is
            // being read is queued behind it rather than lost.
            let changes = watchers.subscribe(peer);
            let owner = peer.to_string();
            let snapshot = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .iter()
                .filter(|task| task.operator_id() == Some(owner.as_str()))
                .map(TaskSummary::of)
                .collect();
            Ok(super::server::Watching { snapshot, changes })
        })
    }

    fn remove(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<(), String>> {
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            let owner = peer.to_string();
            let task = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or("that task is no longer here")?;
            if task.operator_id() != Some(owner.as_str()) {
                return Err("that task belongs to another operator".into());
            }
            tasks
                .remove_and_wait(task)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

impl super::tracker::Host for OwnTasks {
    fn start_issue(
        &self,
        peer: EndpointId,
        issue: IssueSnapshot,
        label: String,
    ) -> super::tracker::HostFuture<'_, String> {
        Box::pin(async move {
            if !self.accept_delegated_work {
                return Err("This machine is not accepting delegated work. Turn on \
                     `accept_delegated_work` in its serve.toml and restart it."
                    .into());
            }
            let owner = peer.to_string();
            let scoped = self
                .config
                .for_operator(&owner)
                .map_err(|error| error.to_string())?;
            if !scoped.settings_path().is_file() {
                return Err("Connect this remote in Altitude Settings and push your setup before delegating work".into());
            }
            let settings = scoped
                .load_or_create_settings()
                .map_err(|error| error.to_string())?;
            let Some(scope) = issue.issue.scope.as_ref() else {
                return Err(
                    "This issue cannot be routed: the tracker did not say which group it \
                     belongs to."
                        .into(),
                );
            };
            // An exact claim beats a project that claims its whole account, so a
            // workspace that grows a second team does not start sending both to
            // whichever project was there first.
            let projects = settings.settings().projects();
            let project = projects
                .iter()
                .find(|project| {
                    project
                        .tracker
                        .as_ref()
                        .is_some_and(|route| route.claims(&scope.id, &scope.aliases))
                })
                .or_else(|| {
                    projects.iter().find(|project| {
                        project
                            .tracker
                            .as_ref()
                            .is_some_and(|route| route.claims_anything())
                    })
                })
                .cloned();
            let Some(project) = project else {
                return Err(format!(
                    "No project on this machine claims {}. Add it to a project's tracker route \
                     in Settings → Projects, then push your setup here.",
                    scope.label
                ));
            };
            if !project
                .tracker
                .as_ref()
                .is_some_and(|route| route.accepts_delegated_work)
            {
                return Err(format!(
                    "Project {:?} claims this team but is not authorized to accept delegated \
                     work. Turn on \"Accept delegated work\" for it in Settings → Projects, then \
                     push your setup here.",
                    project.name
                ));
            }
            let agent = project
                .agent
                .or_else(|| settings.settings().agent())
                .ok_or("Choose an agent in Altitude, then push your setup to this remote")?;
            let task = self
                .tasks
                .create_issue_for_operator(
                    issue,
                    project,
                    agent,
                    InstallationIdentity {
                        id: owner.clone(),
                        label,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(task.id)
        })
    }

    fn stop(&self, task_id: String) -> super::tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            if let Some(task) = self
                .tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| task.id == task_id)
            {
                self.tasks
                    .remove_and_wait(task)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    }

    fn unresolved_signals(
        &self,
        task_id: String,
    ) -> super::tracker::HostFuture<'_, Vec<TaskOperatorSignalThread>> {
        Box::pin(async move {
            self.tasks
                .service()
                .unresolved_signal_threads(&task_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn events(
        &self,
        task_id: String,
    ) -> super::tracker::HostFuture<'_, Vec<(i64, TaskHistoryEvent)>> {
        Box::pin(async move {
            self.tasks
                .events(&task_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn instruct(&self, task_id: String, instruction: String) -> super::tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .update_intent(&task_id, instruction)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn reply(
        &self,
        task_id: String,
        signal_id: String,
        answer: String,
    ) -> super::tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .reply(&task_id, &signal_id, answer)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn accept(&self, task_id: String, signal_id: String) -> super::tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .service()
                .confirm_operator_signal(&task_id, &signal_id)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn is_active(&self, task_id: String) -> super::tracker::HostFuture<'_, bool> {
        Box::pin(async move {
            Ok(self
                .tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .iter()
                .any(|task| task.id == task_id))
        })
    }
}

/// Wait until the machine's runtime can make sandboxes, or say why it cannot.
async fn wait_until_ready(runtime: &mut watch::Receiver<IsolationStatus>) -> Result<(), String> {
    loop {
        match &*runtime.borrow_and_update() {
            IsolationStatus::Ready => return Ok(()),
            IsolationStatus::Failed(error) => return Err(error.clone()),
            _ => {}
        }
        runtime.changed().await.map_err(|e| e.to_string())?;
    }
}

async fn transfer_committed(
    tasks: &TaskEventLoop<MicrosandboxIsolation>,
    prepared: &PreparedTransfer,
) -> Result<bool, String> {
    let history = tasks
        .events(&prepared.handover.task_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(history.iter().any(|(_, event)| {
        matches!(event,
            TaskHistoryEvent::TransferCommitted { transfer_id, recipient }
            if transfer_id == &prepared.handover.transfer_id && recipient == &prepared.recipient)
    }))
}

fn is_machine_local_event(event: &TaskHistoryEvent) -> bool {
    matches!(
        event,
        TaskHistoryEvent::TaskSnapshotSaved { .. }
            | TaskHistoryEvent::TrackerSynced { .. }
            | TaskHistoryEvent::ExecutionRecovered { .. }
            | TaskHistoryEvent::ExecutionPaused { .. }
            | TaskHistoryEvent::TransferStaged { .. }
            | TaskHistoryEvent::TransferCommitted { .. }
            | TaskHistoryEvent::TransferLanded { .. }
            | TaskHistoryEvent::TransferAbandoned { .. }
            | TaskHistoryEvent::SandboxProvisioned { .. }
            | TaskHistoryEvent::SandboxRemoved { .. }
            | TaskHistoryEvent::SnapshotRemoved { .. }
    )
}

async fn bundle_capsule(
    task: &Task,
    directory: &Path,
) -> Result<(TransferArchive, Option<super::EnvironmentArchive>), String> {
    let Some(path) = task
        .facts
        .snapshot_path
        .as_ref()
        .filter(|_| task.facts.snapshot_available)
    else {
        return Ok((
            TransferArchive {
                len: 0,
                digest: blake3::hash(&[]).to_hex().to_string(),
            },
            None,
        ));
    };
    let snapshot = Snapshot::open(path).await.map_err(|e| e.to_string())?;
    let encoded = snapshot.labels().get(ENVIRONMENT_LABEL).ok_or_else(|| {
        "the capsule has no recorded environment boundary; rebuild its task state explicitly before transferring".to_owned()
    })?;
    let base: EnvironmentBase = serde_json::from_str(encoded).map_err(|e| e.to_string())?;
    let staging = tempfile::tempdir_in(directory).map_err(|e| e.to_string())?;
    let (environment, capsule) = base.stage(&snapshot, staging.path()).await?;
    // Both receivers prepare the image locally before importing. Bundling its
    // cache would also carry a VMDK descriptor with the sender's absolute paths.
    environment
        .save_to(&directory.join("environment.tar.zst"), SaveOpts::default())
        .await
        .map_err(|e| e.to_string())?;
    capsule
        .save_to(
            &directory.join("capsule.tar.zst"),
            SaveOpts {
                since: Some(
                    environment
                        .path()
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                        .into_owned(),
                ),
                ..SaveOpts::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    let descriptor_digest = environment.digest().to_owned();
    let snapshot_id = environment.manifest().snapshot_id.to_string();
    Ok((
        archive_description(&directory.join("capsule.tar.zst"))?,
        Some(super::EnvironmentArchive {
            snapshot_id,
            descriptor_digest,
            layer_roots: base.roots,
            archive: archive_description(&directory.join("environment.tar.zst"))?,
        }),
    ))
}

fn archive_description(path: &Path) -> Result<TransferArchive, String> {
    Ok(TransferArchive {
        len: fs::metadata(path).map_err(|e| e.to_string())?.len(),
        digest: file_digest(path)?,
    })
}

fn file_digest(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub async fn run(args: ServeArgs) -> Result<(), Box<dyn Error>> {
    let base = match &args.data_dir {
        Some(dir) => Config::load()?.with_data_dir(dir.clone()),
        None => Config::load()?,
    };
    let config = base.serve();
    let settings_path = args
        .config
        .clone()
        .unwrap_or_else(|| config.serve_settings_path());
    // Everything below happens instead of serving, and before the log is pointed
    // at stdout: what it prints is the answer, not a report about it.
    if let Some(command) = &args.command {
        match command {
            ServeCommand::Clients => {
                super::admin::list(&config)
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            ServeCommand::Evict { key } => {
                super::admin::evict(&config, key)
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            // A service definition carries this machine's own paths, so it is
            // written by the process that has them rather than from a template
            // somebody downloaded.
            ServeCommand::Install => return super::service::install(&args, &base).await,
            ServeCommand::Uninstall => return super::service::uninstall(&args, &base).await,
            ServeCommand::Connect(connect) => {
                let settings = ServeSettings::load_or_create(&settings_path)?;
                let credentials_path = config.data_dir().join("trackers/linear/oauth.json");
                return match connect.tracker {
                    Tracker::Linear => {
                        super::tracker_setup::connect_linear(
                            &settings,
                            &settings_path,
                            &credentials_path,
                        )
                        .await
                    }
                };
            }
        }
    }

    let settings = ServeSettings::load_or_create(&settings_path)?;
    let credentials_path = config.data_dir().join("trackers/linear/oauth.json");
    logging::init_service();
    tracing::info!("⛰  altitude v{} · remote", env!("CARGO_PKG_VERSION"));
    tracing::debug!("Checking sandbox host requirements");
    MicrosandboxIsolation::check_host()?;
    tracing::debug!("Sandbox host requirements passed");
    tracing::debug!(settings = %settings_path.display(), data = %config.data_dir().display(), "Remote configuration");

    let listen: SocketAddr = args
        .listen
        .as_deref()
        .unwrap_or(&settings.listen)
        .parse()
        .map_err(|error| {
            io::Error::other(format!(
                "{} is not an address and port: {error}",
                args.listen.as_deref().unwrap_or(&settings.listen)
            ))
        })?;

    let mut linear_runtime = None;
    let mut linear_error = None;
    if let Some(linear) = settings.linear.clone() {
        match connect_tracker(&config, &linear, &credentials_path).await {
            Ok(runtime) => linear_runtime = Some(runtime),
            // Reported once, below, with what to run about it.
            Err(error) => linear_error = Some(error),
        }
    }
    // A machine that will never run Linear must not tell every client that Linear
    // is missing, so only a configured tracker is announced. An app that was never
    // installed is worth telling apart from one that will not sign in: they send
    // its owner to different steps.
    let readiness = settings.linear.as_ref().map(|_| match &linear_runtime {
        Some(_) => TrackerReadiness::Ready,
        None if oauth::installed(&credentials_path).is_some() => TrackerReadiness::Unavailable,
        None => TrackerReadiness::NotInstalled,
    });
    let trackers = readiness
        .map(|state| {
            vec![TrackerConnection {
                provider: TrackerKind::Linear,
                state,
            }]
        })
        .unwrap_or_default();
    match (readiness, &linear_runtime) {
        (Some(_), Some(runtime)) => tracing::info!(
            "{}: workspace {} · webhooks on {}",
            TrackerKind::Linear.label(),
            runtime.workspace_name,
            runtime
                .listener
                .local_addr()
                .map(|address| address.to_string())
                .unwrap_or_default(),
        ),
        (Some(state), None) => tracing::warn!(
            error = linear_error.as_deref().unwrap_or("no reason given"),
            "{}: {} · run `altitude serve connect linear` on this machine",
            TrackerKind::Linear.label(),
            state.said(),
        ),
        (None, _) => tracing::info!(
            "Task tracker: none configured · run `altitude serve connect linear` to pilot work \
             from Linear"
        ),
    }

    let identity = InstallationKey::load_or_create(config.data_dir().join("identity.key"))
        .map_err(io::Error::other)?;
    let clients =
        ClientRecord::open(config.data_dir().join("clients.json")).map_err(io::Error::other)?;
    let label = config.identity().map_err(io::Error::other)?.label;
    let isolation = MicrosandboxIsolation::new(&config);
    let prepared = isolation::prepare(isolation.clone());
    // This machine's own answer to "can you be given work", which is not the same
    // question as "is the runtime up": it is ready once its runtime can make
    // sandboxes. Environments are built per operator, after a push or before a
    // run, rather than from a shared configuration at startup.
    let (state, observed) = watch::channel(RemoteState::Preparing);
    let database = Database::open(&config).await.map_err(io::Error::other)?;
    let tasks = TaskService::new(
        TaskStore::new(&database),
        isolation,
        config.clone(),
        AgentEngines::new(),
        config.identity().map_err(io::Error::other)?,
    );
    // A remote with no Linear app of its own still reports task status, through
    // the login the task's own peer synced here. When the app is configured,
    // agent sessions own reporting and this would be a second voice.
    let reports_through_pushed_logins = linear_runtime.is_none();
    let mut tasks = tasks.with_issue_reader(
        linear_runtime
            .as_ref()
            .map(|runtime| runtime.automation.clone() as Arc<dyn IssueReader>),
    );
    if reports_through_pushed_logins {
        tasks = tasks.with_trackers(Arc::new(PushedTrackers::new(config.clone())));
    }
    let (task_loop, mut updates) = TaskEventLoop::waiting_for_runtime(tasks);
    let task_loop = Arc::new(task_loop);
    let watchers = Arc::new(Watchers::default());
    {
        // A machine that finishes preparing says so, rather than leaving the
        // clients that asked before it was ready believing it never got there.
        let watchers = Arc::clone(&watchers);
        let mut state = state.subscribe();
        tokio::spawn(async move {
            while state.changed().await.is_ok() {
                if *state.borrow_and_update() == RemoteState::Ready {
                    watchers.ready();
                }
            }
        });
    }
    {
        let watchers = Arc::clone(&watchers);
        let tasks = Arc::clone(&task_loop);
        tokio::spawn(async move {
            while let Some(update) = updates.recv().await {
                match update {
                    // Bytes for a terminal are not a card: what a watcher reads is
                    // the task, and only the events that carry one change it.
                    TaskExecutionUpdate::Output { .. } => {}
                    TaskExecutionUpdate::Created(Ok(task))
                    | TaskExecutionUpdate::Started(Ok(task))
                    | TaskExecutionUpdate::Finished(_, Ok(task))
                    | TaskExecutionUpdate::OperatorReady(task)
                    | TaskExecutionUpdate::Report { task, .. } => watchers.publish(&task),
                    TaskExecutionUpdate::Removed(id, _) => watchers.removed(id),
                    // A failure that came before a task existed carries no task
                    // with it, so what changed is read from the store rather than
                    // guessed at. Errors are rare; a card left saying the wrong
                    // thing is not.
                    TaskExecutionUpdate::Created(Err(_))
                    | TaskExecutionUpdate::Started(Err(_))
                    | TaskExecutionUpdate::Finished(_, Err(_)) => {
                        if let Ok(all) = tasks.list().await {
                            for task in &all {
                                watchers.publish(task);
                            }
                        }
                    }
                }
            }
        });
    }
    // What this machine can mount is what a pushed configuration may carry,
    // besides the tracker profiles that only travel.
    let mounted_profiles: Vec<&'static str> = task_loop
        .service()
        .credentials()
        .profiles()
        .iter()
        .map(|profile| profile.profile)
        .collect();
    if reports_through_pushed_logins {
        let syncing = Arc::clone(&task_loop);
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Err(error) = syncing.service().sync_tracker().await {
                    tracing::warn!(%error, "tracker updates remain queued");
                }
            }
        });
    }
    let incoming = IncomingTransfers::new(config.clone(), task_loop.clone());
    let host = Arc::new(OwnTasks {
        config: Arc::new(config.clone()),
        tasks: task_loop,
        incoming,
        preparation_lock: Arc::new(AsyncMutex::new(())),
        activity: Arc::new(Mutex::new(HashMap::new())),
        watchers,
        read_slots: Arc::new(Semaphore::new(4)),
        readiness: state,
        runtime: prepared,
        tracker: linear_runtime
            .as_ref()
            .map(|runtime| runtime.tracker.clone()),
        accept_delegated_work: settings.accept_delegated_work,
    });
    if let Some(runtime) = linear_runtime {
        let tracker = runtime.tracker.clone();
        let delegated_host: Arc<dyn super::tracker::Host> = host.clone();
        tokio::spawn(tracker.run(delegated_host));
        tokio::spawn(async move {
            if let Err(error) =
                axum::serve(runtime.listener, runtime.tracker.webhook_router()).await
            {
                tracing::error!(%error, "Linear webhook listener stopped");
            }
        });
    }
    let server = LinkServer::serve(ServerParts {
        identity,
        clients,
        listen,
        label,
        data_dir: config.data_dir().to_owned(),
        mounted_profiles,
        state: observed,
        trackers,
        tasks: host.clone(),
    })
    .await
    .map_err(io::Error::other)?;
    let ticket = server.ticket().map_err(io::Error::other)?;
    let connect_address = if listen.ip().is_unspecified() {
        whoami::hostname()
            .ok()
            .filter(|host| !host.is_empty())
            .map(|host| {
                if listen.port() == DEFAULT_PORT {
                    host
                } else {
                    format!("{host}:{}", listen.port())
                }
            })
    } else {
        Some(listen.to_string())
    };

    tracing::info!(address = %listen, "Listening · TCP discovery and UDP task connections · preparing sandboxes");
    tracing::debug!(identity = %server.id(), %ticket, "Remote connection details");

    // Readiness is behind the server, so the answer to "what can you do" is
    // available while the answer is still "not yet".
    let mut runtime = host.runtime.clone();
    let readiness = host.readiness.clone();
    let resumed = host.tasks.clone();
    let preparation = async {
        loop {
            let status = runtime.borrow_and_update().clone();
            match status {
                IsolationStatus::Starting => tracing::info!("Starting sandbox runtime"),
                IsolationStatus::InstallingRuntime => {
                    tracing::info!("Installing sandbox runtime · first startup may take a moment")
                }
                IsolationStatus::PreparingBaseImage => {
                    tracing::info!("Preparing base sandbox image")
                }
                IsolationStatus::PreparingBasePackages => {
                    tracing::info!("Installing base tools")
                }
                IsolationStatus::Failed(error) => return Err(io::Error::other(error)),
                IsolationStatus::Ready => break,
            }
            runtime.changed().await.map_err(io::Error::other)?;
        }
        tracing::info!("Sandbox ready · recovering saved tasks");
        for result in resumed.service().recover_all().await {
            if let Err(error) = result {
                tracing::warn!(%error, "a task needs attention after restart");
            }
        }
        resumed.enable_admission();
        readiness.send_replace(RemoteState::Ready);
        tracing::info!("Remote ready · open Altitude → Settings → Remotes");
        if !listen.ip().is_loopback() {
            tracing::info!("Clients on the same network can discover this remote automatically");
        }
        if let Some(address) = &connect_address {
            tracing::info!(address = %address, "To add it manually, enter this address");
        } else {
            tracing::info!("To add it manually, enter this machine’s hostname");
        }
        Ok::<(), io::Error>(())
    };
    tokio::pin!(preparation);

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    // A transfer nobody is reading is a task nobody is running, and the only
    // thing that can tell is the machine holding it.
    let mut transfers = interval(Duration::from_secs(30));
    transfers.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut is_ready = false;
    let result = loop {
        tokio::select! {
            _ = transfers.tick(), if is_ready => host.release_stalled_transfers().await,
            result = &mut preparation, if !is_ready => match result {
                Ok(()) => is_ready = true,
                Err(error) => {
                    tracing::error!(%error, logs = %config.isolation_data_dir().join("sandboxes/altitude-base-image-setup/logs").display(), "Remote startup failed · no tasks will be accepted");
                    break Err(error);
                }
            },
            _ = terminate.recv() => break Ok(()),
            _ = interrupt.recv() => break Ok(()),
        }
    };

    tracing::info!("Stopping remote · suspending active tasks");
    // Suspension comes before the links close: every task's disk is flushed to
    // this host first, so a link that refuses to shut down cleanly is a smaller
    // loss than work that never reached a snapshot.
    let suspended = host.tasks.suspend_all().await;
    let stopped = server.shutdown().await.map_err(io::Error::other);
    result?;
    suspended?;
    stopped?;
    tracing::info!("Remote stopped");
    Ok(())
}

/// The tracker each peer synced, opened from that peer's own profile.
///
/// A remote has no login of its own, so it reports a task through the account of
/// the peer that owns the task. Opening one is cheap and the session inside it is
/// shared, so the tracker is kept once opened — until the peer signs in again,
/// which replaces the login the open one was made from.
struct PushedTrackers {
    config: Config,
    opened: Mutex<HashMap<String, Opened>>,
}

struct Opened {
    tracker: Arc<Linear>,
    signed_in_at: Option<SystemTime>,
}

impl PushedTrackers {
    fn new(config: Config) -> Self {
        Self {
            config,
            opened: Mutex::new(HashMap::new()),
        }
    }
}

impl TrackerHost for PushedTrackers {
    fn for_operator(&self, operator: &str) -> Option<Arc<dyn TrackerProvider>> {
        if let Some(opened) = self.opened.lock().ok()?.get(operator)
            && opened.signed_in_at == opened.tracker.sign_in_changed_at()
        {
            return Some(opened.tracker.clone() as Arc<dyn TrackerProvider>);
        }
        let directory = self
            .config
            .for_operator(operator)
            .ok()?
            .profile_dir(TrackerKind::Linear.profile());
        let tracker = Linear::from_directory(&directory).ok()?;
        // A peer that pushed a tracker profile without signing in has nothing to
        // report through, and is not an error worth warning about every pass.
        if !tracker.is_signed_in() {
            return None;
        }
        let signed_in_at = tracker.sign_in_changed_at();
        let tracker = Arc::new(tracker);
        self.opened.lock().ok()?.insert(
            operator.to_owned(),
            Opened {
                tracker: Arc::clone(&tracker),
                signed_in_at,
            },
        );
        Some(tracker as Arc<dyn TrackerProvider>)
    }
}

struct LinearRuntime {
    automation: Arc<LinearAutomation>,
    tracker: Arc<super::tracker::RemoteTracker>,
    listener: TcpListener,
    /// What the install said it was, so startup can name it instead of saying
    /// only that something is connected.
    workspace_name: String,
}

async fn connect_tracker(
    config: &Config,
    settings: &LinearSettings,
    credentials_path: &Path,
) -> Result<LinearRuntime, String> {
    let mut credentials = oauth::AppCredentials::load(credentials_path)?;
    credentials
        .refresh_if_needed(settings, credentials_path)
        .await?;
    let workspace_name = credentials.workspace_name.clone();
    let automation = Arc::new(
        credentials
            .client()
            .await
            .map_err(|error| error.to_string())?,
    );
    let listener = TcpListener::bind(&settings.listen)
        .await
        .map_err(|error| format!("could not listen for Linear webhooks: {error}"))?;
    let tracker = super::tracker::RemoteTracker::new(super::tracker::RemoteTrackerConfig {
        automation: automation.clone(),
        webhook_secret: settings.webhook_secret()?.into_bytes(),
        oauth_client_id: settings.client_id.clone(),
        storage_path: config.data_dir().join("trackers/linear/deliveries.json"),
        operators_dir: config.data_dir().join("operators"),
    })?;
    let settings = settings.clone();
    let path = credentials_path.to_owned();
    let refreshed_automation = automation.clone();
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            match credentials.refresh_if_needed(&settings, &path).await {
                Ok(true) => {
                    if let Err(error) =
                        refreshed_automation.replace_token(credentials.access_token().to_owned())
                    {
                        tracing::warn!(%error, "Linear login needs attention");
                    }
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "Linear login refresh remains queued"),
            }
        }
    });
    Ok(LinearRuntime {
        automation,
        tracker,
        listener,
        workspace_name,
    })
}

pub(super) async fn export_task<E>(
    config: &Config,
    tasks: &TaskEventLoop<E>,
    task_id: &str,
    recipient: &str,
) -> Result<Handover, String>
where
    E: IsolationEngine + Clone + 'static,
{
    let root = config.data_dir().join("transfers");
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(&root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let Ok(bytes) = fs::read(entry.path().join("transfer.json")) else {
            continue;
        };
        let prepared: PreparedTransfer =
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if prepared.handover.task_id == task_id {
            if prepared.recipient != recipient {
                return Err("another recipient is already taking this task".into());
            }
            return Ok(prepared.handover);
        }
    }
    let owned = tasks.list().await.map_err(|e| e.to_string())?;
    let task = owned
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| "this machine does not own that task".to_owned())?;
    let held = tasks
        .events(task_id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .rev()
        .find_map(|(_, event)| match event {
            TaskHistoryEvent::TransferStaged {
                transfer_id, peer, ..
            } => Some((transfer_id, peer)),
            _ => None,
        });
    let transfer_id =
        if task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer) {
            let (id, staged_recipient) =
                held.ok_or_else(|| "the staged transfer has no recipient".to_owned())?;
            if staged_recipient != recipient {
                return Err("another recipient is already taking this task".into());
            }
            id
        } else {
            if owned
                .iter()
                .filter(|task| {
                    task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer)
                })
                .count()
                >= 4
            {
                return Err("four transfers are already awaiting confirmation".into());
            }
            uuid::Uuid::new_v4().to_string()
        };
    let directory = root.join(&transfer_id);
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    for name in ["capsule.tar.zst", "environment.tar.zst"] {
        let _ = fs::remove_file(directory.join(name));
    }
    let stopped = tasks
        .stage_transfer(task_id, &transfer_id, recipient)
        .await
        .map_err(|e| e.to_string())?;
    let events = tasks
        .events(&stopped.id)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|(_, event)| !is_machine_local_event(event))
        .map(|(recorded_at_ms, event)| RecordedEvent {
            recorded_at_ms,
            event,
        })
        .collect();
    let record = super::HandoverRecord {
        draft: HandoverDraft {
            intent: stopped.facts.intent.clone(),
            project: stopped
                .project()
                .cloned()
                .ok_or("the task has no recorded project")?,
            agent: stopped.agent_kind.unwrap_or_default(),
            agent_conversation_id: stopped.facts.agent_conversation_id.clone(),
        },
        events,
    };
    let history_path = directory.join("history.json");
    let file = fs::File::create(&history_path).map_err(|e| e.to_string())?;
    serde_json::to_writer(&file, &record).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    let history = archive_description(&history_path)?;
    if history.len > super::protocol::MAX_BLOB_BYTES {
        return Err("task history exceeds the transfer limit".into());
    }
    let (capsule, environment) = bundle_capsule(&stopped, &directory).await?;
    let handover = Handover {
        task_id: task_id.to_owned(),
        transfer_id,
        history,
        capsule,
        environment,
    };
    let prepared = PreparedTransfer {
        recipient: recipient.to_owned(),
        handover: handover.clone(),
    };
    let file =
        fs::File::create(directory.join("transfer.json.incoming")).map_err(|e| e.to_string())?;
    serde_json::to_writer(&file, &prepared).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(
        directory.join("transfer.json.incoming"),
        directory.join("transfer.json"),
    )
    .map_err(|e| e.to_string())?;
    Ok(handover)
}
