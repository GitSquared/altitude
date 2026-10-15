//! The serve role: work for clients, with no terminal.
//!
//! A different execution mode, not a different configuration of the same one. A
//! serve role has nobody at the keyboard, so it never takes the terminal, it logs to
//! stdout for whatever service manager started it, and it reads its own settings
//! file rather than the client's.

use std::{
    error::Error,
    fs, io,
    io::{Read, Seek, SeekFrom},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    isolation::capsule::{ENVIRONMENT_LABEL, EnvironmentBase},
    task::TaskExecutionPauseReason,
};
use iroh::EndpointId;
use microsandbox::{SaveOpts, Snapshot};
use tokio::{
    sync::{Mutex as AsyncMutex, Semaphore},
    task::spawn_blocking,
};

use super::send::IncomingTransfers;
use super::{Handover, HandoverDraft, RecordedEvent, TransferArchive};

use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::watch,
    time::interval,
};

use crate::{
    agent::AgentEngines,
    cli::ServeArgs,
    config::{Config, InstallationIdentity, LinearSettings, ServeSettings},
    db::Database,
    isolation::{self, IsolationEngine, IsolationStatus, MicrosandboxIsolation},
    logging,
    remote::protocol::{TaskRequest, TaskSummary},
    remote::{
        ClientRecord, InstallationKey, LinkServer,
        announce::RemoteState,
        server::{BoxFuture, ServerParts, TaskHost},
    },
    task::{Task, TaskEventLoop, TaskService, TaskStore},
    task::{TaskHistoryEvent, TaskOperatorSignal},
    tracker::{IssueReader, IssueSnapshot, automation::LinearAutomation, oauth},
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PreparedTransfer {
    recipient: String,
    handover: Handover,
}

struct OwnTasks {
    config: Arc<Config>,
    tasks: Arc<TaskEventLoop<MicrosandboxIsolation>>,
    incoming: IncomingTransfers,
    preparation_lock: Arc<AsyncMutex<()>>,
    read_slots: Arc<Semaphore>,
    readiness: watch::Sender<RemoteState>,
    runtime: watch::Receiver<IsolationStatus>,
    tracker: Option<Arc<super::tracker::RemoteTracker>>,
}

impl OwnTasks {
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
    fn signals(
        &self,
        peer: EndpointId,
        task_id: String,
    ) -> BoxFuture<Result<Vec<TaskOperatorSignal>, String>> {
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
                .operator_signals(&task.id)
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
                SignalCommand::Read { task_id } => tasks
                    .list()
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .find(|task| task.id == *task_id),
                SignalCommand::Acknowledge { signal_id }
                | SignalCommand::Answer { signal_id, .. }
                | SignalCommand::Accept { signal_id } => tasks
                    .service()
                    .task_for_signal(signal_id)
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
                SignalCommand::Read { task_id } => {
                    tasks
                        .service()
                        .read_operator_signals(&task_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                SignalCommand::Acknowledge { signal_id } => {
                    tasks
                        .service()
                        .acknowledge_operator_signal(&signal_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                SignalCommand::Answer { signal_id, answer } => tasks
                    .reply(&signal_id, answer)
                    .await
                    .map_err(|error| error.to_string())?,
                SignalCommand::Accept { signal_id } => {
                    tasks
                        .service()
                        .confirm_operator_signal(&signal_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        })
    }

    fn pair_tracker(
        &self,
        peer: EndpointId,
        tracker_user_id: String,
    ) -> BoxFuture<Result<String, String>> {
        let tracker = self.tracker.clone();
        Box::pin(async move {
            tracker.ok_or("Linear automation is not configured on this remote; ask its infrastructure owner to connect it")?
            .begin_pair(peer, tracker_user_id)
        })
    }

    fn hand_over(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<Handover, String>> {
        let tasks = self.tasks.clone();
        let config = self.config.clone();
        let lock = self.preparation_lock.clone();
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
            export_task(&config, &tasks, &task_id, &peer.to_string()).await
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
            for environment in settings.settings().environments() {
                tasks
                    .prepare_environment_as_operator(&owner, &environment, agent)
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
            let environment = settings
                .settings()
                .environments()
                .into_iter()
                .find(|environment| environment.name == request.environment)
                .ok_or_else(|| {
                    format!("this machine has no {} environment", request.environment)
                })?;
            tasks
                .prepare_environment_as_operator(&owner, &environment, request.agent)
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
                        .create(intent, environment, request.agent, Some(operator))
                        .await
                        .map_err(|error| error.to_string())?;
                    tasks.admit();
                }
                super::protocol::TaskOrigin::Issue(issue) => {
                    // A piloted session needs a proved tracker user on this
                    // remote, so refuse before a capsule exists rather than
                    // running work nobody can answer.
                    if let Some(tracker) = &tracker
                        && !tracker.is_peer_paired(peer)
                    {
                        return Err(
                            "pair this remote with your Linear account before delegating issue work"
                                .into(),
                        );
                    }
                    let task = tasks
                        .create_issue_for_operator(*issue, environment, request.agent, operator)
                        .await
                        .map_err(|error| error.to_string())?;
                    if let Some(tracker) = &tracker
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
                .map(summarise)
                .collect())
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
            let agent = settings
                .settings()
                .agent()
                .ok_or("Choose an agent in Altitude, then push your setup to this remote")?;
            let environment = settings
                .settings()
                .environments()
                .into_iter()
                .find(|environment| {
                    environment
                        .name
                        .eq_ignore_ascii_case(&settings.settings().default_environment)
                })
                .ok_or("The pushed default environment is missing; push your setup again")?;
            let task = self
                .tasks
                .create_issue_for_operator(
                    issue,
                    environment,
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

    fn signals(&self, task_id: String) -> super::tracker::HostFuture<'_, Vec<TaskOperatorSignal>> {
        Box::pin(async move {
            self.tasks
                .service()
                .operator_signals(&task_id)
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

    fn reply(&self, signal_id: String, answer: String) -> super::tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .reply(&signal_id, answer)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn accept(&self, signal_id: String) -> super::tracker::HostFuture<'_, ()> {
        Box::pin(async move {
            self.tasks
                .service()
                .confirm_operator_signal(&signal_id)
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

fn summarise(task: &Task) -> TaskSummary {
    let mut extras = Vec::new();
    if task.facts.unread_operator_signal_count > 0 {
        extras.push(format!(
            "{} unread",
            task.facts.unread_operator_signal_count
        ));
    }
    TaskSummary {
        id: task.id.clone(),
        host: task.host.clone(),
        title: task.facts.title.clone(),
        intent: task
            .facts
            .intent
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        environment: task.facts.environment_name.clone(),
        agent: task.agent_kind,
        status: task.facts.execution_status.clone(),
        mode: task.facts.execution_mode,
        tracker_issue: task.facts.tracker_issue.clone(),
        operator_label: task.facts.operator_label.clone(),
        latest_turn_summary: task.facts.latest_turn_summary.clone(),
        extras,
    }
}

pub async fn run(args: ServeArgs) -> Result<(), Box<dyn Error>> {
    logging::init_service();

    let config = match args.data_dir {
        Some(dir) => Config::load()?.with_data_dir(dir).serve(),
        None => Config::load()?.serve(),
    };
    let settings_path = args.config.unwrap_or_else(|| config.serve_settings_path());
    let settings = ServeSettings::load_or_create(&settings_path)?;
    let credentials_path = config.data_dir().join("trackers/linear/oauth.json");
    if args.connect_linear {
        let linear = settings.linear.ok_or_else(|| io::Error::other("Add [linear] client_id and public_url to serve.toml, then set ALTITUDE_LINEAR_CLIENT_SECRET and ALTITUDE_LINEAR_WEBHOOK_SECRET on this host"))?;
        return oauth::connect(linear, &credentials_path)
            .await
            .map_err(|error| io::Error::other(error).into());
    }

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
    if let Some(linear) = settings.linear.clone() {
        match connect_tracker(&config, &linear, &credentials_path).await {
            Ok(runtime) => linear_runtime = Some(runtime),
            Err(error) => {
                tracing::warn!(%error, "Linear needs setup; this remote can still run tasks from Altitude")
            }
        }
    }
    // Announced to clients: a configured [linear] block is not enough, the app
    // must have initialized, which is what the runtime above means.
    let linear_ready = linear_runtime.is_some();

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
        config.task_snapshots_dir(),
        AgentEngines::new(),
        config.identity().map_err(io::Error::other)?,
        config.agent_channel_dir(),
    );
    let tasks = tasks.with_issue_reader(
        linear_runtime
            .as_ref()
            .map(|runtime| runtime.automation.clone() as Arc<dyn IssueReader>),
    );
    let (task_loop, mut updates) = TaskEventLoop::waiting_for_runtime(tasks);
    tokio::spawn(async move {
        while updates.recv().await.is_some() {
            tracing::debug!("a task changed");
        }
    });
    let task_loop = Arc::new(task_loop);
    let incoming = IncomingTransfers::new(config.clone(), task_loop.clone());
    let host = Arc::new(OwnTasks {
        config: Arc::new(config.clone()),
        tasks: task_loop,
        incoming,
        preparation_lock: Arc::new(AsyncMutex::new(())),
        read_slots: Arc::new(Semaphore::new(4)),
        readiness: state,
        runtime: prepared,
        tracker: linear_runtime
            .as_ref()
            .map(|runtime| runtime.tracker.clone()),
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
        state: observed,
        linear_ready,
        tasks: host.clone(),
    })
    .await
    .map_err(io::Error::other)?;
    let ticket = server.ticket().map_err(io::Error::other)?;

    tracing::info!(
        listening = %listen,
        settings = %settings_path.display(),
        data = %config.data_dir().display(),
        "serving"
    );
    println!("listening on {listen} (udp)");
    println!("settings     {}", settings_path.display());
    println!("data         {}", config.data_dir().display());
    println!("identity     {}", server.id());
    println!("ticket       {ticket}");

    // Readiness is behind the server, so the answer to "what can you do" is
    // available while the answer is still "not yet".
    let mut runtime = host.runtime.clone();
    let readiness = host.readiness.clone();
    let resumed = host.tasks.clone();
    tokio::spawn(async move {
        match wait_until_ready(&mut runtime).await {
            Ok(()) => {
                for result in resumed.service().recover_all().await {
                    if let Err(error) = result {
                        tracing::warn!(%error, "a task needs attention after restart");
                    }
                }
                resumed.enable_admission();
                readiness.send_replace(RemoteState::Ready);
            }
            Err(error) => tracing::error!(%error, "the isolation runtime never became ready"),
        }
    });

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = terminate.recv() => tracing::info!("asked to stop"),
        _ = interrupt.recv() => tracing::info!("asked to stop"),
    }

    server.shutdown().await.map_err(io::Error::other)?;
    host.tasks.suspend_all().await?;
    Ok(())
}

struct LinearRuntime {
    automation: Arc<LinearAutomation>,
    tracker: Arc<super::tracker::RemoteTracker>,
    listener: TcpListener,
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
            environment: stopped.environment(),
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
