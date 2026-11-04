use crate::remote::archive;
use crate::remote::protocol::{self};
use crate::remote::transfer as update;
use crate::remote::{HandoverRecord, TRANSCRIPT_FILE, WORK_FILE, WORK_PLAIN_FILE};

use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};

use crate::{
    isolation::{FileTally, FileTransferSink},
    task::TaskExecutionPauseReason,
};

use crate::remote::{Handover, HandoverDraft, RecordedEvent, TransferArchive};

use crate::{
    config::Config,
    isolation::{IsolationEngine, MicrosandboxIsolation},
    task::TaskHistoryEvent,
    task::{Task, TaskEventLoop},
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PreparedTransfer {
    pub(super) recipient: String,
    pub(super) handover: Handover,
}

pub(super) const TRANSFER_IDLE_MS: i64 = 120_000;

pub(super) struct WatchedCollect {
    pub(super) task_id: String,
    pub(super) collecting: Arc<Mutex<HashMap<String, update::TransferUpdate>>>,
}

pub(super) async fn transfer_committed(
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
            // Facts about the desk this copy was at, which the machine receiving the
            // task is not: the arrival describes itself with its own landing.
            | TaskHistoryEvent::TaskArrivedFrom { .. }
            | TaskHistoryEvent::TaskHandedBack
    )
}

async fn bundle_task_files<E>(
    tasks: &TaskEventLoop<E>,
    task: &Task,
    directory: &Path,
    progress: &dyn FileTransferSink,
) -> Result<(TransferArchive, TransferArchive), String>
where
    E: IsolationEngine + Clone + 'static,
{
    let plain = directory.join(WORK_PLAIN_FILE);
    let packed = directory.join(WORK_FILE);
    let empty = || TransferArchive {
        len: 0,
        digest: blake3::hash(&[]).to_hex().to_string(),
    };
    tasks
        .pack_task_files(task, directory, progress)
        .await
        .map_err(|error| error.to_string())?;
    let work = if plain.is_file() {
        let started = Instant::now();
        let unpacked = archive::pack(plain.clone(), packed.clone())
            .await
            .map_err(|error| format!("the working files could not be compressed: {error}"))?;
        let _ = fs::remove_file(&plain);
        let described = archive_description(&packed)?;
        tracing::info!(
            task = %task.id,
            files_bytes = unpacked,
            packed_bytes = described.len,
            elapsed_ms = started.elapsed().as_millis(),
            "packed a task's working files into one archive"
        );
        described
    } else {
        empty()
    };
    let transcript = if directory.join(TRANSCRIPT_FILE).is_file() {
        archive_description(&directory.join(TRANSCRIPT_FILE))?
    } else {
        empty()
    };
    Ok((work, transcript))
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

impl FileTransferSink for WatchedCollect {
    fn counted(&self, whole: FileTally) {
        self.record(|update| {
            update.total = whole.files;
            update.done = 0;
            update.unit = update::Unit::Files;
            update.stage = "collecting the working files…".to_owned();
        });
    }

    fn moved(&self, step: FileTally) {
        self.record(|update| update.done = update.done.saturating_add(step.files));
    }
}

impl WatchedCollect {
    fn record(&self, change: impl FnOnce(&mut update::TransferUpdate)) {
        if let Ok(mut collecting) = self.collecting.lock() {
            let update = collecting.entry(self.task_id.clone()).or_default();
            change(update);
        }
    }
}

pub(crate) async fn export_task<E>(
    config: &Config,
    tasks: &TaskEventLoop<E>,
    task_id: &str,
    recipient: &str,
    progress: &dyn FileTransferSink,
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
        let Ok(prepared) = serde_json::from_slice::<PreparedTransfer>(&bytes) else {
            continue;
        };
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
    tracing::info!(
        task = %task_id,
        %transfer_id,
        %recipient,
        "packing a task to hand to another machine"
    );
    for name in [WORK_FILE, TRANSCRIPT_FILE] {
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
    let record = HandoverRecord {
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
    if history.len > protocol::MAX_BLOB_BYTES {
        return Err("task history exceeds the transfer limit".into());
    }
    let (work, transcript) = bundle_task_files(tasks, &stopped, &directory, progress).await?;
    let handover = Handover {
        task_id: task_id.to_owned(),
        transfer_id,
        history,
        work,
        transcript,
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
