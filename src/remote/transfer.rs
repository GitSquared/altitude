use crate::isolation::{FileTally, FileTransferSink};
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

    pub fn counting_files(&self, total: u64) {
        self.updates.send_modify(|update| {
            update.stage.clear();
            update.done = 0;
            update.total = total;
            update.unit = Unit::Files;
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

impl FileTransferSink for Transfer {
    fn counted(&self, whole: FileTally) {
        self.counting_files(whole.files);
    }

    fn moved(&self, step: FileTally) {
        let moved = step.files;
        self.updates
            .send_modify(|update| update.done = update.done.saturating_add(moved));
    }
}
