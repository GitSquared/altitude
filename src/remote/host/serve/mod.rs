use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{task::TaskExecutionPauseReason, vcs::now_ms};
use iroh::EndpointId;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::remote::send::IncomingTransfers;
use crate::remote::transfer::TransferUpdate;
use crate::remote::{HISTORY_FILE, TRANSCRIPT_FILE, WORK_FILE, WatchEvent};

use tokio::sync::{mpsc, watch};

use crate::{
    config::Config,
    isolation::{IsolationStatus, MicrosandboxIsolation},
    remote::announce::Phase,
    remote::protocol::{self, TaskSummary},
    task::{QueueStanding, Task, TaskEventLoop, TaskHistoryEvent, waits_for_room},
};

use crate::tracker::TrackerIntegration;

mod run;
pub(crate) mod settings;
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
    transfers: Arc<Transfers>,
    collecting: Arc<Mutex<HashMap<String, TransferUpdate>>>,
    watchers: Arc<Watchers>,
    read_slots: Arc<Semaphore>,
    readiness: watch::Sender<Phase>,
    runtime: watch::Receiver<IsolationStatus>,
    tracker: Option<Arc<dyn TrackerIntegration>>,
    delegation_paused: bool,
}

// When a transfer in flight would be stale if it said nothing more. A transfer that is
// moving pushes that moment out, and the server sleeps until it: with nothing in
// flight there is no moment to wake for, so nothing does.
pub(super) struct Transfers {
    moving: Mutex<HashMap<String, Instant>>,
    deadline: watch::Sender<Option<Instant>>,
}

impl Transfers {
    pub(super) fn new() -> Self {
        let (deadline, _) = watch::channel(None);
        Self {
            moving: Mutex::new(HashMap::new()),
            deadline,
        }
    }

    // The furthest-out moment allows the whole set: a transfer that just moved must
    // not be declared stale because a different one has been quiet for a while.
    fn arm(&self) -> Option<Instant> {
        let window = Duration::from_millis(TRANSFER_IDLE_MS as u64);
        let next = self
            .moving
            .lock()
            .expect("transfer activity lock poisoned")
            .values()
            .map(|moved| *moved + window)
            .min();
        self.deadline.send_replace(next);
        next
    }

    fn moved(&self, transfer_id: &str) {
        self.moving
            .lock()
            .expect("transfer activity lock poisoned")
            .insert(transfer_id.to_owned(), Instant::now());
        self.arm();
    }

    fn last(&self, transfer_id: &str) -> Option<Instant> {
        self.moving
            .lock()
            .expect("transfer activity lock poisoned")
            .get(transfer_id)
            .copied()
    }

    fn finished(&self, transfer_id: &str) {
        self.moving
            .lock()
            .expect("transfer activity lock poisoned")
            .remove(transfer_id);
        self.arm();
    }

    pub(super) fn watch(&self) -> watch::Receiver<Option<Instant>> {
        self.deadline.subscribe()
    }
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
    // The sender comes back with the receiver because a client can open a watch on
    // a machine that is already ready, and the one word that says so was broadcast
    // to the clients listening when the build finished.
    fn subscribe(
        &self,
        peer: EndpointId,
    ) -> (
        mpsc::UnboundedSender<WatchEvent>,
        mpsc::UnboundedReceiver<WatchEvent>,
    ) {
        let (changes, receiving) = mpsc::unbounded_channel();
        let mut peers = self.peers.lock().expect("watchers lock poisoned");
        peers.retain(|watching| !watching.changes.is_closed());
        peers.push(WatchingPeer {
            peer,
            changes: changes.clone(),
        });
        (changes, receiving)
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

    fn publish(&self, operator: &str, summary: TaskSummary) {
        self.changed(operator, summary);
    }
}

// What a client is told about one task: the task itself, and where it stands when it is
// waiting for room. The numbering is the machine's, so a client cannot work it out.
fn summary_of(task: &Task, standing: Option<&QueueStanding>) -> protocol::TaskSummary {
    let mut summary = protocol::TaskSummary::of(task);
    summary.ahead = standing.map(|standing| standing.ahead);
    summary.holding = standing.and_then(|standing| standing.holding);
    summary
}

// A card for one task, and then the queue again: one start or finish moves every
// number behind it, and the numbering is the machine's to say.
async fn publish_change(
    watchers: &Watchers,
    tasks: &TaskEventLoop<MicrosandboxIsolation>,
    task: &Task,
) {
    if let Some(operator) = task.operator_id() {
        watchers.publish(operator, summary_of(task, None));
    }
    publish_queue(watchers, tasks).await;
}

// One start moves every number behind it, so a change anywhere in the queue is said to
// every task still waiting in it.
async fn publish_queue(watchers: &Watchers, tasks: &TaskEventLoop<MicrosandboxIsolation>) {
    let Ok(all) = tasks.service().list_in_play().await else {
        return;
    };
    let standings = tasks.standings(&all);
    for task in all.iter().filter(|task| waits_for_room(task)) {
        let Some(operator) = task.operator_id() else {
            continue;
        };
        watchers.publish(operator, summary_of(task, standings.get(&task.id)));
    }
}

impl OwnTasks {
    // The moment a transfer in flight would be stale, so the server can sleep until it
    // instead of asking every half minute whether there is one.
    pub(super) fn watch_transfers(&self) -> watch::Receiver<Option<Instant>> {
        self.transfers.watch()
    }

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
            let staged = Duration::from_millis((now_ms() - staged_at) as u64);
            let last = self
                .transfers
                .last(&transfer_id)
                .map(|moved| moved.elapsed())
                .unwrap_or(staged);
            if last < Duration::from_millis(TRANSFER_IDLE_MS as u64) {
                continue;
            }
            if let Err(error) = self.tasks.abandon_transfer(&task.id, &transfer_id).await {
                tracing::warn!(task = %task.id, %transfer_id, %error, "could not take a stalled transfer back");
                continue;
            }
            tracing::info!(task = %task.id, %transfer_id, "a transfer stopped moving; the task stays here");
            self.transfers.finished(&transfer_id);
            if let Ok(directory) = self.transfer_directory(&transfer_id) {
                for name in [WORK_FILE, TRANSCRIPT_FILE, HISTORY_FILE] {
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
