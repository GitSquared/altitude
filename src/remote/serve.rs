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
    sync::{Arc, Mutex},
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

use super::{Handover, HandoverDraft, RecordedEvent, TransferArchive};

use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

use crate::{
    agent::AgentEngines,
    cli::ServeArgs,
    config::{Config, InstallationIdentity, ServeSettings},
    db::Database,
    isolation::{self, IsolationStatus, MicrosandboxIsolation},
    logging,
    remote::protocol::{TaskRequest, TaskSummary},
    remote::{
        ClientRecord, InstallationKey, LinkServer,
        announce::RemoteState,
        bundle::HeldConfig,
        server::{BoxFuture, ServerParts, TaskHost},
    },
    task::TaskHistoryEvent,
    task::{Task, TaskEventLoop, TaskService, TaskStore},
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PreparedTransfer {
    recipient: String,
    handover: Handover,
}

struct OwnTasks {
    config: Arc<Config>,
    tasks: Arc<TaskEventLoop<MicrosandboxIsolation>>,
    preparation_lock: Arc<AsyncMutex<()>>,
    read_slots: Arc<Semaphore>,
    readiness: watch::Sender<RemoteState>,
    runtime: watch::Receiver<IsolationStatus>,
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
    fn hand_over(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<Handover, String>> {
        let tasks = Arc::clone(&self.tasks);
        let preparation_lock = Arc::clone(&self.preparation_lock);
        let root = self.config.data_dir().join("transfers");
        Box::pin(async move {
            let _preparing = preparation_lock.lock().await;
            fs::create_dir_all(&root).map_err(|e| e.to_string())?;
            for entry in fs::read_dir(&root).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let Ok(bytes) = fs::read(entry.path().join("transfer.json")) else {
                    continue;
                };
                let prepared: PreparedTransfer =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                if prepared.handover.task_id == task_id {
                    if prepared.recipient != peer.to_string() {
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
                .events(&task_id)
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
                    let (id, recipient) =
                        held.ok_or_else(|| "the staged transfer has no recipient".to_owned())?;
                    if recipient != peer.to_string() {
                        return Err("another recipient is already taking this task".into());
                    }
                    id
                } else {
                    if owned
                        .iter()
                        .filter(|task| {
                            task.facts.execution_pause_reason
                                == Some(TaskExecutionPauseReason::Transfer)
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
                .stage_transfer(&task_id, &transfer_id, &peer.to_string())
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
                task_id,
                transfer_id,
                history,
                capsule,
                environment,
            };
            let prepared = PreparedTransfer {
                recipient: peer.to_string(),
                handover: handover.clone(),
            };
            let file = fs::File::create(directory.join("transfer.json.incoming"))
                .map_err(|e| e.to_string())?;
            serde_json::to_writer(&file, &prepared).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(
                directory.join("transfer.json.incoming"),
                directory.join("transfer.json"),
            )
            .map_err(|e| e.to_string())?;
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

    fn configure(&self) -> BoxFuture<Result<(), String>> {
        let config = Arc::clone(&self.config);
        let tasks = Arc::clone(&self.tasks);
        let state = self.readiness.clone();
        let mut runtime = self.runtime.clone();
        Box::pin(async move {
            state.send_replace(RemoteState::Preparing);
            loop {
                match &*runtime.borrow_and_update() {
                    IsolationStatus::Ready => break,
                    IsolationStatus::Failed(error) => return Err(error.clone()),
                    _ => {}
                }
                runtime.changed().await.map_err(|e| e.to_string())?;
            }
            let settings = config
                .load_or_create_settings()
                .map_err(|e| e.to_string())?;
            let agent = settings.settings().agent().unwrap_or_default();
            for environment in settings.settings().environments() {
                tasks
                    .prepare_environment(&environment, agent)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            state.send_replace(RemoteState::Ready);
            Ok(())
        })
    }

    fn run(&self, request: TaskRequest) -> BoxFuture<Result<(), String>> {
        let config = Arc::clone(&self.config);
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            let settings = config
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
                .prepare_environment(&environment, request.agent)
                .await
                .map_err(|e| e.to_string())?;
            tasks.create(
                request.intent,
                environment,
                request.agent,
                Some(InstallationIdentity {
                    id: request.identity_id,
                    label: request.identity_label,
                }),
            );
            Ok(())
        })
    }

    fn list(&self) -> BoxFuture<Result<Vec<TaskSummary>, String>> {
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            Ok(tasks
                .list()
                .await
                .map_err(|e| e.to_string())?
                .iter()
                .map(summarise)
                .collect())
        })
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
    environment
        .save_to(
            &directory.join("environment.tar.zst"),
            SaveOpts {
                with_image: true,
                ..SaveOpts::default()
            },
        )
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
        intent: task.facts.intent.clone(),
        environment: task.facts.environment_name.clone(),
        agent: task.agent_kind,
        status: task.facts.execution_status.clone(),
        mode: task.facts.execution_mode,
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

    let identity = InstallationKey::load_or_create(config.data_dir().join("identity.key"))
        .map_err(io::Error::other)?;
    let clients =
        ClientRecord::open(config.data_dir().join("clients.json")).map_err(io::Error::other)?;
    let label = config.identity().map_err(io::Error::other)?.label;
    let isolation = MicrosandboxIsolation::new(&config);
    let prepared = isolation::prepare(isolation.clone());
    // This machine's own answer to "can you be given work", which is not the same
    // question as "is the runtime up": it is ready once what the configuration asks
    // for has been built.
    let (state, observed) = watch::channel(RemoteState::Preparing);
    let held = Arc::new(Mutex::new(HeldConfig::load(
        &config.data_dir().join("profiles.json"),
    )));
    let database = Database::open(&config).await.map_err(io::Error::other)?;
    let tasks = TaskService::new(
        TaskStore::new(&database),
        isolation,
        config.task_snapshots_dir(),
        AgentEngines::new(),
        config.identity().map_err(io::Error::other)?,
    );
    let (task_loop, mut updates) = TaskEventLoop::new(tasks);
    tokio::spawn(async move {
        while updates.recv().await.is_some() {
            tracing::debug!("a task changed");
        }
    });
    let host = Arc::new(OwnTasks {
        config: Arc::new(config.clone()),
        tasks: Arc::new(task_loop),
        preparation_lock: Arc::new(AsyncMutex::new(())),
        read_slots: Arc::new(Semaphore::new(4)),
        readiness: state,
        runtime: prepared,
    });
    let server = LinkServer::serve(ServerParts {
        identity,
        clients,
        listen,
        label,
        config_path: config.settings_path(),
        data_dir: config.data_dir().to_owned(),
        state: observed,
        held,
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

    // Built behind the server, so the answer to "what can you do" is available while
    // the answer is still "not yet".
    tokio::spawn(async move {
        if let Err(error) = host.configure().await {
            tracing::error!(%error, "could not prepare the configured environments");
        }
    });

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = terminate.recv() => tracing::info!("asked to stop"),
        _ = interrupt.recv() => tracing::info!("asked to stop"),
    }

    server.shutdown().await.map_err(io::Error::other)?;
    Ok(())
}
