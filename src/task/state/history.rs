use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum TaskHistoryEvent {
    TaskTitleSet {
        title: String,
    },
    // A project's memory is a directory of markdown files the agent edits directly;
    // this records the directory's state as a task leaves a machine, so another
    // machine can write the files it is missing. A path's latest event wins, and
    // `None` is a file that was removed.
    ProjectMemoryFileWritten {
        project_id: String,
        path: String,
        contents: Option<String>,
        written_at_ms: i64,
    },
    IntentExpressed {
        execution_status: TaskExecutionStatus,
        execution_mode: TaskExecutionMode,
        sandbox_available: bool,
        snapshot_available: bool,
        intent: String,
        project: Project,
        identity_id: String,
        identity_label: String,
        agent_conversation_id: Option<String>,
    },
    // A task chosen for a start is claimed before any sandbox work begins. The task
    // stays Pending, so the claim — and not the execution status — is what says this
    // machine is already preparing it and no other caller may. The claim is spent by
    // the AgentRunStarted it produces, or given back by StartRelinquished.
    StartClaimed {
        claim_id: String,
        owner: String,
    },
    // A claim whose preparation ended without a run: the machine holding it has given
    // it back, and the next eligible caller may claim the task again.
    StartRelinquished {
        claim_id: String,
    },
    SandboxProvisioned {
        sandbox_available: bool,
        sandbox_id: String,
    },
    AgentRunStarted {
        execution_status: TaskExecutionStatus,
        agent_run_id: String,
        sandbox_id: String,
        execution_mode: TaskExecutionMode,
        #[serde(default)]
        answering_signal_id: Option<String>,
    },
    ControlRequested {
        requested_mode: TaskExecutionMode,
        agent_run_id: String,
    },
    AgentRunEnded {
        agent_run_id: String,
        outcome: TaskAgentRunOutcome,
        exit_code: Option<i32>,
    },
    AgentModelObserved {
        model: String,
    },
    TaskInputExpressed {
        input: TaskInput,
    },
    PlanUpdated {
        steps: Vec<TaskPlanStep>,
    },
    OperatorSignalRaised {
        signal: TaskAskQuestion,
    },
    OperatorSignalMessage {
        signal_id: String,
        author: TaskAskAuthor,
        body: String,
    },
    // A message from the operator reached a task that was not working, and the task was
    // woken to read it.
    Wake,
    // A message from the operator reached a task that was already working, and the agent
    // was handed it in the turn it is in. Which of the two happened is the loop's to
    // know: nothing else can tell a task that was awake from one that was not.
    Steer,
    OperatorSignalResolved {
        signal_id: String,
        resolved: bool,
        by: TaskAskAuthor,
        // The message that settled the thread, when the agent named one. A tracker
        // reads a closed thread as answered by whatever closed it, and only the agent
        // knows which of its own words closed a thread it withdrew unanswered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolving_message_id: Option<String>,
    },
    OperatorSignalConfirmed {
        signal_id: String,
    },
    CodeContributionLinked {
        contribution: CodeContribution,
    },
    CodeContributionObserved {
        observation: ContributionObservation,
    },
    ReviewDeferred {
        signal: TaskAskQuestion,
    },
    ReviewDeferralCleared {
        signal_id: String,
        reason: String,
    },
    ExecutionPaused {
        execution_status: TaskExecutionStatus,
        sandbox_available: bool,
        snapshot_available: bool,
        execution_pause_reason: TaskExecutionPauseReason,
        snapshot_path: String,
    },
    // The account's limit holding the task, recorded with the pause it explains: what
    // holds it and when it lets go, which is the one wait nobody has to clear.
    LimitHeld {
        window: AgentWindow,
        clears_at_ms: i64,
    },
    AgentUsageReported {
        cost_usd_micros: i64,
        cost_from_provider: bool,
        is_charged: bool,
    },
    AgentStandingReported {
        standing: AgentStanding,
    },
    ExecutionRecovered {
        execution_status: TaskExecutionStatus,
        sandbox_available: bool,
        snapshot_available: bool,
        snapshot_path: String,
    },
    TaskSnapshotSaved {
        sandbox_available: bool,
        snapshot_available: bool,
        snapshot_path: String,
    },
    ExecutionFailed {
        execution_status: TaskExecutionStatus,
        error: String,
    },
    TransferStaged {
        transfer_id: String,
        peer: String,
        execution_status: TaskExecutionStatus,
        execution_pause_reason: TaskExecutionPauseReason,
        execution_mode: TaskExecutionMode,
        sandbox_available: bool,
        snapshot_available: bool,
        snapshot_path: Option<String>,
    },
    TransferLanded {
        transfer_id: String,
        execution_status: TaskExecutionStatus,
        execution_pause_reason: TaskExecutionPauseReason,
    },
    TransferCommitted {
        transfer_id: String,
        recipient: String,
    },
    TransferAbandoned {
        transfer_id: String,
        execution_status: TaskExecutionStatus,
        execution_pause_reason: Option<TaskExecutionPauseReason>,
        snapshot_available: bool,
        snapshot_path: Option<String>,
    },
    ExecutionQueued {
        execution_status: TaskExecutionStatus,
        // What the run opens with, when something asked the task to move again.
        #[serde(default)]
        wake: Option<String>,
    },
    SandboxRemoved {
        sandbox_available: bool,
    },
    SnapshotRemoved {
        snapshot_available: bool,
    },
    TaskCompleted {
        execution_status: TaskExecutionStatus,
    },
    TaskArrivedFrom {
        address: String,
    },
    TaskHandedBack,
    TrackerIssueBound {
        issue: IssueSnapshot,
        #[serde(default)]
        delegation: Option<String>,
    },
    TrackerSynced {
        event_count: usize,
        #[serde(default)]
        status: Option<TrackerStatus>,
    },
}

impl From<&AgentUsage> for TaskHistoryEvent {
    fn from(usage: &AgentUsage) -> Self {
        Self::AgentUsageReported {
            cost_usd_micros: usage.cost_usd_micros,
            cost_from_provider: usage.cost_from_provider,
            is_charged: usage.is_charged,
        }
    }
}

impl TaskHistoryEvent {
    // Ends a review hold only where the caller is already putting a turn in flight:
    // it is written together with the ExecutionQueued that wakes a stopped task, or
    // on a task still Pending from an earlier wake. It must not be used on a running
    // task. The ending reads the hold before it suspends the capsule, so clearing the
    // hold in that gap stops the task for a review it no longer holds, with no turn
    // queued -- and the watcher only observes paused, held tasks.
    pub(crate) fn held_review_cleared(task: &Task, reason: &str) -> Vec<Self> {
        let Some(deferred) = task.facts.deferred_review.clone() else {
            return Vec::new();
        };
        vec![
            Self::ReviewDeferralCleared {
                signal_id: deferred.signal.id.clone(),
                reason: reason.to_owned(),
            },
            Self::OperatorSignalResolved {
                signal_id: deferred.signal.id,
                resolved: true,
                by: TaskAskAuthor::Operator,
                resolving_message_id: None,
            },
        ]
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskPlanStep {
    pub content: String,
    pub status: TaskPlanStatus,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskPlanStatus {
    Pending,
    InProgress,
    Completed,
    Canceled,
}

// One file of a project's memory, as the newest event about it records it. It is a
// materialized view the memory directory is written from, not a second copy kept in
// step by hand.
#[derive(Clone, Debug)]
pub struct ProjectMemoryFile {
    pub path: String,
    pub contents: Option<String>,
    pub at_ms: i64,
}
