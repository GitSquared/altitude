pub(crate) mod automation;
pub(crate) mod remote;

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use crate::{
    agent::Activity,
    isolation::{EnvironmentProgress, Pressure},
    remote::StartFuture,
    task::{TaskAsk, TaskAskKind, TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent},
    tracker::{IssueReader, IssueReference, IssueSnapshot, TrackerStatus},
};
use serde::{Deserialize, Serialize};

pub type DelegatedWorkFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub type HostTask<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackerReadiness {
    Ready,
    NotInstalled,
    Unavailable,
}

impl TrackerReadiness {
    pub fn said(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotInstalled => "not installed",
            Self::Unavailable => "cannot sign in",
        }
    }
}

pub struct BoundTask {
    pub task_id: String,
    pub owner: String,
    pub issue: IssueReference,
    pub delegation: Option<String>,
    pub session_agent: Option<String>,
    pub status: TaskExecutionStatus,
    pub pause_reason: Option<TaskExecutionPauseReason>,
    pub ask: Option<TaskAskKind>,
}

// Where a task stands on the machine it was sent to: what is in front of it in the
// queue, what the machine already has on it, and what is being done to this task if
// the machine has picked it up. One answer, because a queue number alone cannot say
// whether a wait is a queue, a machine with no room, or a start under way.
pub struct QueueStanding {
    pub ahead: usize,
    pub on_machine: usize,
    pub holding: Option<Pressure>,
    pub start: Option<Start>,
}

// A start is minutes of work before the first sandbox exists, and the steps of an
// environment build are what fills them. Calling that a wait for a sandbox told the
// operator nothing about a machine that was working the whole time.
pub enum Start {
    Provisioning,
    Building(EnvironmentProgress),
}

// What a tracker integration asks of the machine that runs the work. Every method
// names an act in the tracker's vocabulary and leaves the task's own record to the
// machine.
pub trait DelegatedWork: Send + Sync + 'static {
    fn start_issue(
        &self,
        peer: iroh::EndpointId,
        issue: IssueSnapshot,
        label: String,
        delegation: Option<String>,
    ) -> StartFuture<'_, String>;
    fn stop(
        &self,
        task_id: String,
        reason: TaskExecutionPauseReason,
    ) -> DelegatedWorkFuture<'_, ()>;
    fn complete(&self, task_id: String) -> DelegatedWorkFuture<'_, ()>;
    fn record_status(&self, task_id: String, status: TrackerStatus) -> DelegatedWorkFuture<'_, ()>;
    fn signal_threads(&self, task_id: String) -> DelegatedWorkFuture<'_, Vec<TaskAsk>>;
    // Closing and reopening are one act with a flag: the operator's word on whether a
    // thread needs anything more, said the same way from the interface and the tracker.
    fn set_signal_resolved(
        &self,
        task_id: String,
        signal_id: String,
        resolved: bool,
    ) -> DelegatedWorkFuture<'_, ()>;
    fn events(&self, task_id: String) -> DelegatedWorkFuture<'_, Vec<(i64, TaskHistoryEvent)>>;
    fn steer(&self, task_id: String, message: String) -> DelegatedWorkFuture<'_, ()>;
    fn reply(
        &self,
        task_id: String,
        signal_id: String,
        answer: String,
    ) -> DelegatedWorkFuture<'_, ()>;
    fn accept(&self, task_id: String, signal_id: String) -> DelegatedWorkFuture<'_, ()>;
    fn is_kept(&self, task_id: String) -> DelegatedWorkFuture<'_, bool>;

    fn bound_tasks(&self) -> DelegatedWorkFuture<'_, Vec<BoundTask>>;
    fn delegation_of(&self, task_id: String) -> DelegatedWorkFuture<'_, Option<String>>;
    fn work_for_issue(
        &self,
        issue_id: String,
        delegations: Vec<String>,
    ) -> DelegatedWorkFuture<'_, Option<BoundTask>>;
    fn session_agent(&self, task_id: String) -> DelegatedWorkFuture<'_, Option<String>>;

    fn queue_standings(&self) -> DelegatedWorkFuture<'_, HashMap<String, QueueStanding>>;
    fn current_activity(&self, task_id: String) -> DelegatedWorkFuture<'_, Option<Activity>>;
}

// A tracker integration running on a remote: the webhook listener and the passes
// that turn what arrived into work. What it asks of the machine is above; this is
// what the machine asks of it.
pub trait TrackerIntegration: Send + Sync + 'static {
    fn workspace_label(&self) -> &str;
    fn webhook_address(&self) -> &str;
    // A tracker integration that can read issues itself, for the machine's own
    // queue and the work it starts. One that reports through pushed logins has none.
    fn issue_reader(&self) -> Option<Arc<dyn IssueReader>>;
    // Anything that changed what the tracker owes: a delivery arrived, or a task's
    // history moved.
    fn wake(&self);
    fn forget_task<'a>(&'a self, task_id: &'a str) -> HostTask<'a>;
    fn announce_shutdown<'a>(&'a self, work: &'a dyn DelegatedWork) -> HostTask<'a>;
    // Starts the webhook listener and the event loop. The caller has already built
    // the work it will be told about.
    fn run(self: Arc<Self>, work: Arc<dyn DelegatedWork>);
}
