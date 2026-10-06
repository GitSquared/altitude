//! Moving a task capsule between nodes.
//!
//! A task is owned by exactly one node, and only that node may run it. A
//! transfer hands ownership to another node. Two rules make that safe:
//!
//! 1. The source stops running the task before the payload leaves it.
//! 2. The destination owns the task only once it holds the whole payload.
//!
//! Break either rule and two nodes can run the same task. Keep both and the
//! worst case is that nobody runs it for a moment, which is self-repairing.
//!
//! Pure. No I/O, no database, no network. See `design/decisions.md` (Q12) for why
//! authority is possession rather than a version number.

#![allow(dead_code)] // Remove once this module is wired into the application.

use std::{error::Error, fmt};

/// Which node owns a task, and whether a transfer is in flight.
///
/// The owning node holds the task and may run it. While a transfer is in flight
/// nobody runs it: the source has given it up and the destination does not own
/// it yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskNodeOwnership {
    owning_node: String,
    transfer_destination_node: Option<String>,
}

impl TaskNodeOwnership {
    /// A task held by `owning_node`, going nowhere.
    pub fn owned_by_node(owning_node: &str) -> Self {
        Self {
            owning_node: owning_node.to_owned(),
            transfer_destination_node: None,
        }
    }

    pub fn owning_node(&self) -> &str {
        &self.owning_node
    }

    /// Where the task is being sent, while a transfer is in flight.
    pub fn transfer_destination_node(&self) -> Option<&str> {
        self.transfer_destination_node.as_deref()
    }

    pub fn is_transferring(&self) -> bool {
        self.transfer_destination_node.is_some()
    }

    pub fn is_owned_by(&self, node: &str) -> bool {
        self.owning_node == node
    }

    /// Whether `node` may run the task right now.
    ///
    /// This is the question the rest of the system asks. An in-flight transfer
    /// is what stops the source from running the task while the payload travels.
    pub fn node_may_run_task(&self, node: &str) -> bool {
        !self.is_transferring() && self.is_owned_by(node)
    }

    /// The source stops running the task and starts sending it to
    /// `destination_node`.
    ///
    /// Retrying the send is the caller's business; starting the transfer is done
    /// once, and doing it twice is a mistake worth reporting.
    pub fn start_transfer_to_node(
        &mut self,
        destination_node: &str,
    ) -> Result<(), TaskTransferError> {
        if self.is_transferring() {
            return Err(TaskTransferError::TransferAlreadyInFlight);
        }
        if self.is_owned_by(destination_node) {
            return Err(TaskTransferError::DestinationAlreadyOwnsTask);
        }
        self.transfer_destination_node = Some(destination_node.to_owned());
        Ok(())
    }

    /// The destination holds the whole task, so it now owns it.
    ///
    /// Only the destination calls this, and only after the payload is durably
    /// there. That is rule 2: owning the task before holding it would let it run
    /// in two places.
    pub fn destination_took_ownership(&mut self) -> Result<(), TaskTransferError> {
        let destination_node = self
            .transfer_destination_node
            .take()
            .ok_or(TaskTransferError::NoTransferInFlight)?;
        self.owning_node = destination_node;
        Ok(())
    }

    /// The payload did not reach the destination, so the source keeps the task
    /// and runs it again.
    pub fn payload_did_not_reach_destination(&mut self) -> Result<(), TaskTransferError> {
        if !self.is_transferring() {
            return Err(TaskTransferError::NoTransferInFlight);
        }
        self.transfer_destination_node = None;
        Ok(())
    }
}

/// Why a transfer could not be started or finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskTransferError {
    TransferAlreadyInFlight,
    NoTransferInFlight,
    DestinationAlreadyOwnsTask,
}

impl fmt::Display for TaskTransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransferAlreadyInFlight => formatter.write_str("a transfer is already in flight"),
            Self::NoTransferInFlight => formatter.write_str("no transfer is in flight"),
            Self::DestinationAlreadyOwnsTask => {
                formatter.write_str("the destination already owns the task")
            }
        }
    }
}

impl Error for TaskTransferError {}
