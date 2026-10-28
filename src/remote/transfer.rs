//! Counting the bytes of a transfer, for the interface that watches one.
//!
//! The side moving the bytes says how far it has got and checks whether it has
//! been asked to stop; the side drawing it reads that and turns it into a rate.
//! Nothing here knows what an archive is or where it is going.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;

pub const STOPPED: &str = "the transfer was stopped";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferUpdate {
    /// What the transfer is waiting on while it has nothing to count. Empty
    /// while bytes are moving, because what is moving is the task itself.
    pub stage: String,
    pub done: u64,
    /// The whole of what this transfer will move. Zero until that is known,
    /// which is what makes the bar indeterminate.
    pub total: u64,
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
    /// Asks the transfer to stop. It stops between pieces, so this is a request
    /// rather than an interruption.
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

    /// Says how many bytes this transfer will move in all, and that they are
    /// moving now.
    ///
    /// The count already made is kept: a transfer that carries three archives
    /// fills one bar, so the operator watches a task arrive rather than three
    /// archives travel.
    pub fn counting(&self, total: u64) {
        self.updates.send_modify(|update| {
            update.stage.clear();
            update.total = total;
        });
    }

    /// Says what the transfer is waiting on instead of counting.
    ///
    /// What has been counted is kept, so a wait in the middle of a transfer leaves
    /// the bar where it was rather than taking it away and bringing it back lower.
    pub fn waiting(&self, stage: impl Into<String>) {
        let stage = stage.into();
        self.updates.send_modify(|update| update.stage = stage);
    }

    pub fn advanced(&self, bytes: u64) {
        self.updates.send_modify(|update| update.done += bytes);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
}
