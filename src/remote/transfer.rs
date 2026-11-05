use crate::isolation::{EnvironmentProgress, EnvironmentProgressSink, FileTally, FileTransferSink};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;

pub const STOPPED: &str = "the transfer was stopped";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferUpdate {
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub unit: Unit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Unit {
    #[default]
    Bytes,
    Files,
}

pub struct Transfer {
    updates: watch::Sender<TransferUpdate>,
    stopped: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct Watching {
    pub updates: watch::Receiver<TransferUpdate>,
    stopped: Arc<AtomicBool>,
}

impl Watching {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

impl Transfer {
    pub fn start(stage: impl Into<String>) -> (Self, Watching) {
        let stopped = Arc::new(AtomicBool::new(false));
        let (updates, watching) = watch::channel(TransferUpdate {
            stage: stage.into(),
            done: 0,
            total: 0,
            unit: Unit::default(),
        });
        (
            Self {
                updates,
                stopped: Arc::clone(&stopped),
            },
            Watching {
                updates: watching,
                stopped,
            },
        )
    }

    pub fn counting(&self, total: u64) {
        self.updates.send_modify(|update| {
            update.stage.clear();
            update.total = total;
            update.unit = Unit::Bytes;
        });
    }

    pub fn waiting(&self, stage: impl Into<String>) {
        let stage = stage.into();
        self.updates.send_modify(|update| update.stage = stage);
    }

    pub fn watching(&self) -> Watching {
        Watching {
            updates: self.updates.subscribe(),
            stopped: Arc::clone(&self.stopped),
        }
    }

    pub fn watcher(&self) -> Self {
        Self {
            updates: self.updates.clone(),
            stopped: Arc::clone(&self.stopped),
        }
    }

    pub fn reported(&self, update: TransferUpdate) {
        self.updates.send_modify(|held| *held = update);
    }

    pub fn advanced(&self, bytes: u64) {
        self.updates.send_modify(|update| update.done += bytes);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
}

impl EnvironmentProgressSink for Transfer {
    // A cold take spends its minutes here, so the bar says which step rather than
    // standing still through all of them.
    fn report(&self, progress: EnvironmentProgress) {
        self.waiting(format!(
            "{} · step {} of {}",
            progress.label,
            progress.completed + 1,
            progress.total
        ));
    }
}

impl FileTransferSink for Transfer {
    // A tree moves in one step, so there is no fraction of it to report. What the
    // bar has counted so far is the capsule arriving, which is still true.
    fn moving(&self, whole: FileTally) {
        self.waiting(format!("putting {} files in…", whole.files));
    }
}
