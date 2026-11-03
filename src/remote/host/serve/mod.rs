use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::{task::TaskExecutionPauseReason, vcs::now_ms};
use iroh::EndpointId;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::remote::WatchEvent;
use crate::remote::send::IncomingTransfers;
use crate::remote::transfer::TransferUpdate;

use tokio::sync::{mpsc, watch};

use crate::{
    config::Config,
    isolation::{IsolationStatus, MicrosandboxIsolation},
    remote::announce::Phase,
    remote::protocol::TaskSummary,
    task::{Task, TaskEventLoop, TaskHistoryEvent},
};

use crate::remote::host::tracker;

mod run;
mod task_host;
mod tracker_host;
mod transfer;

use transfer::TRANSFER_IDLE_MS;

pub use run::run;
pub(crate) use transfer::export_task;

struct OwnTasks {
    config: Arc<Config>,
    tasks: Arc<TaskEventLoop<MicrosandboxIsolation>>,
    incoming: IncomingTransfers,
    preparation_lock: Arc<AsyncMutex<()>>,
    activity: Arc<Mutex<HashMap<String, i64>>>,
    collecting: Arc<Mutex<HashMap<String, TransferUpdate>>>,
    watchers: Arc<Watchers>,
    read_slots: Arc<Semaphore>,
    readiness: watch::Sender<Phase>,
    runtime: watch::Receiver<IsolationStatus>,
    tracker: Option<Arc<tracker::RemoteTracker>>,
    delegation_paused: bool,
}

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
                for name in ["work.tar.zst", "transcript.jsonl", "history.json"] {
                    let _ = fs::remove_file(directory.join(name));
                }
            }
        }
    }

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
