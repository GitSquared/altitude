use chrono::{Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::agent::{AgentKind, AgentStanding, AgentUsage, AgentWindow, TaskUsage};
use crate::id;
use crate::project::Project;
use crate::tracker::{IssueReference, IssueSnapshot, TrackerKind, TrackerStatus};
use crate::vcs::{CodeContribution, ContributionObservation};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskAgentTurnReport {
    #[serde(default)]
    pub operator_signal: Option<TaskOperatorSignalDraft>,
    #[serde(default)]
    pub model: Option<String>,
}

impl TaskAgentTurnReport {
    pub fn requested_action(&self) -> TaskAgentRequestedAction {
        match self.operator_signal.as_ref().map(|signal| signal.kind) {
            Some(TaskOperatorSignalKind::Blocked | TaskOperatorSignalKind::InReview) => {
                TaskAgentRequestedAction::AwaitOperator
            }
            _ => TaskAgentRequestedAction::Continue,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
// A harness may ask for the operator, but only the operator closes a task out, so
// there is deliberately no way to finish here.
pub enum TaskAgentRequestedAction {
    Continue,
    AwaitOperator,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorSignalDraft {
    pub kind: TaskOperatorSignalKind,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskInput {
    pub id: String,
    pub at_ms: i64,
    pub author: TaskInputAuthor,
    pub source: TaskInputSource,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responding_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TaskInputAuthor {
    Operator { id: String, label: String },
    Participant { id: String, label: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskInputSource {
    TaskCreated,
    Altitude,
    TrackerComment {
        provider: TrackerKind,
        issue_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        document_id: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
// Update is a log that asks for nothing, Decision is an assumption the operator may
// challenge while the work continues, and Blocked is the only kind that stops it.
pub enum TaskOperatorSignalKind {
    Update,
    Decision,
    InReview,
    Blocked,
}

impl TaskOperatorSignalKind {
    pub fn tier(self) -> u8 {
        match self {
            Self::Blocked | Self::InReview => 0,
            Self::Decision => 1,
            Self::Update => 2,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskOperatorAttentionLevel {
    Nominal,
    Requested,
    Alarm,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionPauseReason {
    AwaitingOperator,
    AwaitingReviewChecks,
    ApplicationShutdown,
    // A turn that ends by asking for another one and leaves nothing behind would say the
    // same thing again from the same state.
    AgentWentQuiet,
    // The only pause nobody is waiting on: the account's limit clears by itself, so
    // the task comes back without the operator doing anything.
    AwaitingLimit,
    Transfer,
    // The endings a tracker hands down keep the task: what happened elsewhere is something
    // the operator can read here, rather than work that quietly disappears.
    TrackerEnded,
    StoppedInTracker,
    DelegationWithdrawn,
}

impl TaskExecutionPauseReason {
    pub fn is_out_of_play(&self) -> bool {
        matches!(
            self,
            Self::TrackerEnded | Self::StoppedInTracker | Self::DelegationWithdrawn
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionMode {
    Autopilot,
    AutopilotReport,
    Operator,
}

#[derive(Clone, Debug)]
pub struct TaskAgentRun {
    pub id: String,
    pub task_id: String,
    pub answering_signal_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorSignal {
    pub id: String,
    pub task_id: String,
    pub kind: TaskOperatorSignalKind,
    pub title: String,
    pub body: String,
    pub raised_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProjectProposal>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectProposal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caches: Option<Vec<String>>,
}

impl ProjectProposal {
    pub fn is_empty(&self) -> bool {
        self.setup_command.is_none() && self.caches.is_none()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorSignalThread {
    pub id: String,
    pub task_id: String,
    pub kind: TaskOperatorSignalKind,
    pub title: String,
    pub opened_at_ms: i64,
    pub last_raised_at_ms: i64,
    pub messages: Vec<TaskOperatorSignalMessage>,
    pub resolved: bool,
    pub resolved_by: Option<TaskOperatorSignalAuthor>,
    pub resolved_at_ms: Option<i64>,
    #[serde(default)]
    pub transitions: Vec<SignalTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProjectProposal>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct SignalTransition {
    pub at_ms: i64,
    pub resolved: bool,
    pub by: TaskOperatorSignalAuthor,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorSignalMessage {
    #[serde(default)]
    pub id: String,
    pub author: TaskOperatorSignalAuthor,
    pub body: String,
    pub at_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOperatorSignalAuthor {
    #[default]
    Agent,
    Operator,
}

impl TaskOperatorSignalThread {
    pub fn is_resolved(&self) -> bool {
        self.resolved
    }

    pub fn is_unresolved(&self) -> bool {
        !self.resolved
    }

    pub fn latest_agent_message(&self) -> Option<&TaskOperatorSignalMessage> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.author == TaskOperatorSignalAuthor::Agent)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredReview {
    pub signal: TaskOperatorSignal,
    pub agent_run_id: String,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub agent_kind: Option<AgentKind>,
    pub host: String,
    pub code_contributions: Vec<CodeContribution>,
    pub code_contribution_observations: Vec<ContributionObservation>,
    pub facts: TaskFacts,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskFacts {
    pub accepted: bool,
    pub intent: String,
    pub title: Option<String>,
    pub operator_id: Option<String>,
    pub operator_label: Option<String>,
    #[sqlx(json(nullable))]
    pub project: Option<Project>,
    pub agent_conversation_id: Option<String>,
    pub model: Option<String>,
    #[sqlx(json(nullable))]
    pub plan: Option<Vec<TaskPlanStep>>,
    pub execution_status: TaskExecutionStatus,
    pub execution_pause_reason: Option<TaskExecutionPauseReason>,
    pub execution_mode: TaskExecutionMode,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub sandbox_id: Option<String>,
    pub sandbox_available: bool,
    pub snapshot_path: Option<String>,
    pub snapshot_available: bool,
    // When the account's limit clears, and which of its windows ran out. Read as a
    // time and not a switch: a task is held only while a pause says it is waiting on
    // one, because a value left behind by a later pause would otherwise hold work
    // nobody is waiting on.
    pub resumes_at_ms: Option<i64>,
    pub limit_window: Option<AgentWindow>,
    // What this task has spent, as its harnesses counted it. A sum of every run's
    // report, because a run is what a harness knows the cost of.
    #[sqlx(json)]
    pub usage: TaskUsage,
    // The account this task's credential runs on, as its harness last said. A reading
    // and not a state: it is as old as the run that took it, and the interface says so.
    #[sqlx(json(nullable))]
    pub standing: Option<AgentStanding>,
    pub last_error: Option<String>,
    #[sqlx(json(nullable))]
    pub latest_operator_message: Option<TaskOperatorSignalMessage>,
    #[sqlx(json(nullable))]
    pub tracker_issue: Option<IssueReference>,
    // None means no delegation asked for this task, never one that is unknown: it is
    // what keeps local work out of every question about delegations.
    pub tracker_delegation: Option<String>,
    // The machine this task was carried from to be worked at a desk here. Its work
    // belongs there: it runs only while a person holds it, and it goes back when
    // they stop.
    pub arrived_from: Option<String>,
    #[sqlx(default)]
    pub tracker_status: Option<TrackerStatus>,
    pub blocking_signal_kind: Option<TaskOperatorSignalKind>,
    pub unresolved_signal_count: u32,
    #[sqlx(json(nullable))]
    pub deferred_review: Option<DeferredReview>,
    pub review_wake_reason: Option<String>,
}

impl TaskFacts {
    // A borrowed task is here for a desk, not for the queue: nothing carried to this
    // machine is this machine's to schedule.
    pub fn is_borrowed(&self) -> bool {
        self.arrived_from.is_some()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum TaskHistoryEvent {
    TaskTitleSet {
        title: String,
    },
    IntentExpressed {
        execution_status: TaskExecutionStatus,
        execution_mode: TaskExecutionMode,
        operator_attention_level: TaskOperatorAttentionLevel,
        sandbox_available: bool,
        snapshot_available: bool,
        intent: String,
        project: Project,
        identity_id: String,
        identity_label: String,
        agent_conversation_id: Option<String>,
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
    AgentTurnReported {
        agent_run_id: String,
        requested_action: TaskAgentRequestedAction,
        operator_attention_level: TaskOperatorAttentionLevel,
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
        signal: TaskOperatorSignal,
    },
    OperatorSignalMessage {
        signal_id: String,
        author: TaskOperatorSignalAuthor,
        body: String,
    },
    OperatorSignalResolved {
        signal_id: String,
        resolved: bool,
        by: TaskOperatorSignalAuthor,
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
        signal: TaskOperatorSignal,
        agent_run_id: String,
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
        total_tokens: i64,
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
            total_tokens: usage.total_tokens,
            cost_from_provider: usage.cost_from_provider,
            is_charged: usage.is_charged,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAgentRunOutcome {
    Continued,
    AwaitingOperator,
    // The account refused the turn and said when it clears: nothing about the work
    // went wrong.
    Limited,
    Failed,
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

#[derive(Clone, Debug)]
pub struct TaskAgentTurnContext {
    pub task_id: String,
    pub title: String,
    pub host: String,
    pub project: Option<Project>,
    pub agent: Option<AgentKind>,
    pub model: Option<String>,
    pub plan: Vec<TaskPlanStep>,
    pub execution_status: TaskExecutionStatus,
    pub execution_mode: TaskExecutionMode,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub blocking_signal_kind: Option<TaskOperatorSignalKind>,
    pub unresolved_signal_count: u32,
    pub last_error: Option<String>,
    pub datetime: String,
    pub latest_operator_message: Option<TaskOperatorSignalMessage>,
    pub code_contribution_observations: Vec<ContributionObservation>,
}

impl fmt::Display for TaskAgentTurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "task_id={}; title={:?}; harness={}; model={}; host={}; project={}",
            self.task_id,
            self.title,
            self.agent
                .map(|kind| kind.to_string())
                .unwrap_or_else(|| "none".into()),
            self.model.as_deref().unwrap_or("unknown"),
            self.host,
            self.project
                .as_ref()
                .map(|project| project.name.as_str())
                .unwrap_or("none"),
        )?;
        if let Some(project) = &self.project {
            if let Some(repository) = &project.repository {
                write!(formatter, "repository={repository}; ")?;
            }
            if let Some(checkout) = project.checkout_path() {
                write!(formatter, "checkout={checkout}; ")?;
            }
            writeln!(
                formatter,
                "project_brief=/altitude/project.md; harness=/altitude/harness.md"
            )?;
        }
        writeln!(
            formatter,
            "execution_status={:?}; execution_mode={:?}; attention={:?}; \
             blocking_signal={:?}; unresolved_signals={}; last_error={}",
            self.execution_status,
            self.execution_mode,
            self.operator_attention_level,
            self.blocking_signal_kind,
            self.unresolved_signal_count,
            self.last_error.as_deref().unwrap_or("none"),
        )?;
        write!(formatter, "current_datetime={}", self.datetime)?;
        if !self.plan.is_empty() {
            write!(formatter, "\nplan=")?;
            for (index, step) in self.plan.iter().enumerate() {
                if index > 0 {
                    write!(formatter, "; ")?;
                }
                write!(formatter, "{:?}: {}", step.status, step.content)?;
            }
        }
        if let Some(message) = &self.latest_operator_message {
            write!(
                formatter,
                "\nlatest_operator_message={}",
                serde_json::to_string(message).map_err(|_| fmt::Error)?
            )?;
        }
        if self.code_contribution_observations.is_empty() {
            return Ok(());
        }
        write!(formatter, "\ncode_contributions=")?;
        for (index, observation) in self.code_contribution_observations.iter().enumerate() {
            if index > 0 {
                write!(formatter, "; ")?;
            }
            write!(
                formatter,
                "{} @ {}: {}",
                observation.contribution_id, observation.state.head_sha, observation.summary
            )?;
        }
        Ok(())
    }
}

const TITLE_LIMIT: usize = 48;

pub fn normalize_title(title: &str) -> Option<String> {
    let collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= TITLE_LIMIT {
        return Some(collapsed);
    }
    let mut short = String::new();
    for word in collapsed.split(' ') {
        let separator = usize::from(!short.is_empty());
        if short.chars().count() + separator + word.chars().count() > TITLE_LIMIT {
            break;
        }
        if separator == 1 {
            short.push(' ');
        }
        short.push_str(word);
    }
    // A single word longer than the limit is cut, because a title with no words
    // in it is worse than a truncated one.
    if short.is_empty() {
        return Some(collapsed.chars().take(TITLE_LIMIT).collect());
    }
    Some(short)
}

impl Task {
    pub fn operator_id(&self) -> Option<&str> {
        self.facts.operator_id.as_deref()
    }

    pub fn title(&self) -> &str {
        self.facts
            .title
            .as_deref()
            .unwrap_or_else(|| id::name(&self.id))
    }

    pub fn project(&self) -> Option<&Project> {
        self.facts.project.as_ref()
    }

    pub fn turn_context(&self, execution_mode: TaskExecutionMode) -> TaskAgentTurnContext {
        TaskAgentTurnContext {
            task_id: self.id.clone(),
            title: self.title().to_owned(),
            host: self.host.clone(),
            project: self.facts.project.clone(),
            agent: self.agent_kind,
            model: self.facts.model.clone(),
            plan: self.facts.plan.clone().unwrap_or_default(),
            execution_status: TaskExecutionStatus::Running,
            execution_mode,
            operator_attention_level: self.facts.operator_attention_level.clone(),
            blocking_signal_kind: self.facts.blocking_signal_kind,
            unresolved_signal_count: self.facts.unresolved_signal_count,
            last_error: self.facts.last_error.clone(),
            datetime: Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            latest_operator_message: self.facts.latest_operator_message.clone(),
            code_contribution_observations: self.code_contribution_observations.clone(),
        }
    }

    pub fn session_agent(&self) -> Option<String> {
        let harness = self
            .agent_kind
            .map(|kind| kind.to_string().to_ascii_lowercase());
        let model = self.facts.model.as_deref().map(|model| {
            model
                .rsplit('/')
                .next()
                .unwrap_or(model)
                .to_ascii_lowercase()
        });
        match (harness, model) {
            (Some(harness), Some(model)) => Some(format!("{harness}-{model}")),
            (Some(harness), None) => Some(harness),
            (None, Some(model)) => Some(model),
            (None, None) => None,
        }
    }
}
