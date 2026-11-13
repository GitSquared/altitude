use super::ledger::Cursor;
use crate::tracker::{AskSurface, ask_marker};
use crate::{
    task::{TaskAsk, TaskAskAuthor, TaskAskKind, TaskAskMessage, TaskHistoryEvent, picture},
    tracker::{IssueReference, TrackerStatus, providers::linear::automation::AgentActivityContent},
};
use futures_util::future::join_all;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use tokio::time::timeout;

use crate::agent::ActivityKind;
use crate::brand;
use crate::isolation::Pressure;
use crate::task::TaskExecutionStatus;

use super::*;
use crate::task::TaskPlanStep;
use crate::tracker::TrackerError;
pub(super) struct QueueNotice {
    starting: bool,
    state: String,
    since: Instant,
    at: Instant,
}

mod activity;
mod queue;
mod session;
mod signals;
mod status;
