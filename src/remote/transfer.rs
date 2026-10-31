//! Counting what a transfer has done, for the interface that watches one.
//!
//! The side doing the work says how far it has got and checks whether it has been
//! asked to stop; the side drawing it reads that and turns it into a rate. A
//! transfer moves bytes between machines and files between a machine and a
//! sandbox, so what is counted is whichever of the two it is doing — and the count
//! is what the bar is made of.

use crate::isolation::{FileTally, FileTransferSink};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;

pub const STOPPED: &str = "the transfer was stopped";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferUpdate {
    /// What the transfer is waiting on while it has nothing to count. Empty
    /// while something is moving, because what is moving is the task itself.
    pub stage: String,
    pub done: u64,
    /// The whole of what this transfer will move. Zero until that is known,
    /// which is what makes the bar indeterminate.
    pub total: u64,
    /// What the two numbers count.
    pub unit: Unit,
}

/// What a transfer counts while it works.
///
/// A task crossing machines is measured in bytes; a task crossing a sandbox
/// boundary is measured in files, because an operator waiting on a tree of a few
/// thousand files wants "how many are left", not "how many megabytes".
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
            update.unit = Unit::Bytes;
        });
    }

    /// Says how many files this step will handle, and that it is handling them.
    ///
    /// The count starts again here: files and bytes are different units, and a bar
    /// that carried one into the other would be lying about both.
    pub fn counting_files(&self, total: u64) {
        self.updates.send_modify(|update| {
            update.stage.clear();
            update.done = 0;
            update.total = total;
            update.unit = Unit::Files;
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
