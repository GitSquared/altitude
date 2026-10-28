use chrono::{Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::agent::AgentKind;
use crate::id;
use crate::project::Project;
use crate::tracker::{IssueReference, IssueSnapshot, TrackerKind};
use crate::vcs::{CodeContribution, ContributionObservation};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskAgentTurnReport {
    #[serde(default)]
    pub operator_signal: Option<TaskOperatorSignalDraft>,
    #[serde(default)]
    pub model: Option<String>,
}

impl TaskAgentTurnReport {
    /// What this report asks for next, read from the signal it raises.
    ///
    /// The model used to send this as its own field and could contradict the
    /// signal beside it; the signal already says whether the run waits.
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
/// What a work turn asks for next.
///
/// There is no way to finish a task here, deliberately: a harness may ask for the
/// operator, but only the operator closes a task out.
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

/// One contribution to what the operator has asked for.
///
/// Inputs are the append-only record of expressed intent. The founding brief is
/// the first one; a follow-up message or a signal reply is another. Signals
/// remain the agent's attention requests; an operator's answer to one is an
/// input with `responding_to` set.
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
pub enum TaskOperatorSignalKind {
    /// A durable log of what the agent did. It asks for nothing and is resolved
    /// once the operator has seen it and moved on.
    Update,
    /// An assumption the agent made that a reasonable operator might challenge.
    /// The task continues; the thread is the place to discuss it.
    Decision,
    /// The work is finished as far as the harness can tell, and the operator
    /// decides whether it is done. Accepting closes the task and disposes of its
    /// capsule; a reply sends the task back for more work.
    InReview,
    /// The agent cannot continue without the operator, including when the run
    /// itself stopped. The task is waiting on an answer, not on a particular
    /// cause.
    Blocked,
}

impl TaskOperatorSignalKind {
    /// The order the operator deals with kinds: both blocking ends share the
    /// top, decisions are review context, and updates are the log.
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
    /// The agent asked for review and the host is watching the linked pull
    /// requests. The capsule is in a snapshot, so the wait costs no capacity.
    AwaitingReviewChecks,
    ApplicationShutdown,
    Transfer,
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
    /// The signal thread this run is continuing, when the operator replied on
    /// one and this turn is the agent's answer to it. The report that ends this
    /// run belongs to that thread instead of opening a new one.
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
}

/// One signal thread as it reads now: what the agent raised, and every message
/// exchanged on it since.
///
/// A thread's kind and title are the latest ones the agent gave it, so a
/// conversation can become a review request without losing what led there.
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorSignalMessage {
    /// Position in this thread, as `m1`, `m2`, ... Nothing outside the thread
    /// needs to be unique.
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

    /// The latest thing the agent said on the thread, which is what an outside
    /// system publishing the thread has to show.
    pub fn latest_agent_message(&self) -> Option<&TaskOperatorSignalMessage> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.author == TaskOperatorSignalAuthor::Agent)
    }
}

/// A wrap-up request the host is holding until the linked contributions can
/// answer.
///
/// The agent's run is over, but its request is not delivered yet: the linked
/// pull requests decide whether the operator is asked to review or the agent is
/// woken to fix something. Keeping it as recorded state is what makes the wait
/// survive a restart without an agent turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredReview {
    pub signal: TaskOperatorSignal,
    /// The run that asked for review, recorded so its ending can be written with
    /// the hold that began after it.
    pub agent_run_id: String,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub agent_kind: Option<AgentKind>,
    /// The machine that holds this task's capsule, which is the machine that may run
    /// it. Its own machine until the task is delegated.
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
    /// The key of the operator whose work this is, from the earliest recorded
    /// intent. It scopes their settings, logins and builds on a machine that
    /// serves several peers.
    pub operator_id: Option<String>,
    pub operator_label: Option<String>,
    /// The project this task was founded in, as it was recorded then.
    ///
    /// Recorded rather than looked up: a task carries the setup, repository,
    /// caches and instructions it started with, so it keeps them when the project
    /// is edited or when the task is transferred.
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
    pub last_error: Option<String>,
    #[sqlx(json(nullable))]
    pub latest_operator_message: Option<TaskOperatorSignalMessage>,
    /// The tracker issue this task is bound to, when the operator bound one.
    ///
    /// Derived from the binding event rather than stored twice, so a task that
    /// arrived by transfer carries where it belongs with the rest of its history.
    #[sqlx(json(nullable))]
    pub tracker_issue: Option<IssueReference>,
    pub blocking_signal_kind: Option<TaskOperatorSignalKind>,
    pub unresolved_signal_count: u32,
    /// The wrap-up request the host is holding while it watches linked
    /// contributions, when one is pending.
    #[sqlx(json(nullable))]
    pub deferred_review: Option<DeferredReview>,
    /// Why the host last woke this task from watching its linked contributions.
    ///
    /// Present only until the next run starts, so the reason reaches the turn it
    /// was recorded for and never reads as the reason for a later one.
    pub review_wake_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum TaskHistoryEvent {
    /// The agent renamed the task, which is a fact about what it turned out to be.
    ///
    /// Recorded as history rather than overwritten: the task's earlier names say what
    /// it was mistaken for, and the latest one is what a list reads.
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
        /// The identity this task was delegated under.
        ///
        /// Recorded with the intent because it cannot be inferred later: a machine
        /// that runs work for several operators needs to know whose work this is,
        /// and the answer is only available at the moment of delegation.
        identity_id: String,
        /// How that identity reads to a human, e.g. the machine it was delegated
        /// from. Recorded beside the id so a task's history and a tracker comment
        /// can say where the work is without anyone resolving a key.
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
        /// The thread this run is answering, when the operator started it by
        /// replying to one.
        #[serde(default)]
        answering_signal_id: Option<String>,
    },
    /// The operator asked for the task to be driven by someone else.
    ///
    /// It names the run it was asked during. Only that run can act on it, so a
    /// request cannot land on whichever run happens to end next; a request with
    /// no run to receive it is refused rather than recorded.
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
    /// The model the harness reported for a turn, as the harness names it.
    ///
    /// Observation only: Altitude records what ran, not what should have run.
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
    /// The agent said something on a thread it already opened, or the operator
    /// replied. A message reopens a resolved thread and wakes the agent when the
    /// task is not running; its meaning is the agent's to read.
    OperatorSignalMessage {
        signal_id: String,
        author: TaskOperatorSignalAuthor,
        body: String,
    },
    /// The thread is done, or no longer done. This is the only signal state:
    /// there is no separate read flag.
    OperatorSignalResolved {
        signal_id: String,
        resolved: bool,
        by: TaskOperatorSignalAuthor,
    },
    /// The operator accepted a wrap-up request, which is the only way a task ends.
    /// It resolves the thread as part of the task's ending.
    OperatorSignalConfirmed {
        signal_id: String,
    },
    CodeContributionLinked {
        contribution: CodeContribution,
    },
    /// External state observed for a contribution.
    ///
    /// An input to the task, not a notification: the control loop decides
    /// whether anything about it deserves the operator's attention.
    CodeContributionObserved {
        observation: ContributionObservation,
    },
    /// A wrap-up request the host holds while it watches linked contributions.
    ///
    /// Recorded rather than waited on in memory, so the request is not lost to a
    /// restart and the agent is not asked to poll for it.
    ReviewDeferred {
        signal: TaskOperatorSignal,
        agent_run_id: String,
    },
    /// A held wrap-up request stopped waiting: the contributions need the agent,
    /// or the operator took the task over before the host could decide.
    ReviewDeferralCleared {
        signal_id: String,
        /// Why the wait ended, in the operator's words. The next autopilot turn
        /// reads it as its prompt when the agent is the one being woken.
        reason: String,
    },
    ExecutionPaused {
        execution_status: TaskExecutionStatus,
        sandbox_available: bool,
        snapshot_available: bool,
        execution_pause_reason: TaskExecutionPauseReason,
        snapshot_path: String,
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
    /// A transfer stopped before the source gave the task up.
    ///
    /// The task goes back to waiting to be run here. The pause field is part of
    /// the record even though it is always empty: a reader takes the task's pause
    /// from the latest event that carries the field, so leaving it out would leave
    /// this task recorded as still being transferred.
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
    /// The operator bound this task to a tracker issue when it was founded.
    ///
    /// The snapshot is kept here, once: it is what the task's prompt was built
    /// from, and what the tracker's later state is compared against.
    TrackerIssueBound {
        issue: IssueSnapshot,
    },
    /// How far this task's tracker binding has been reported, by history position.
    ///
    /// `event_count` is the number of history events, checkpoints included, that
    /// existed when the checkpoint was written. A later pass resumes there, so
    /// idle scans never generate a checkpoint of their own.
    TrackerSynced {
        event_count: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAgentRunOutcome {
    Continued,
    AwaitingOperator,
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

/// What Altitude tells an agent about the task it is driving, at the start of a
/// turn.
///
/// This is the whole of what an agent can know about its situation from Altitude:
/// which machine and harness it is, what environment it runs in, what the task is
/// called and how far along it is, and what the operator last said. It is read as
/// prose by the harness, so the display is the interface.
#[derive(Clone, Debug)]
pub struct TaskAgentTurnContext {
    pub task_id: String,
    /// The task's current title, which may still be the intent before the agent
    /// has named it.
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
            writeln!(formatter, "project_brief=/altitude/project.md")?;
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

/// A title, normalized to what a list can show.
///
/// Collapsed to one line and cut at a word boundary, because a title is read
/// rather than parsed. `None` for a title that says nothing, so a blank title
/// never replaces a real one.
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
    /// The key that scopes this task's operator's settings, logins and
    /// environments.
    pub fn operator_id(&self) -> Option<&str> {
        self.facts.operator_id.as_deref()
    }

    /// What a list calls this task: the agent's title, or the task's own id
    /// until the agent has chosen one.
    ///
    /// The id is what an operator can say to an agent and what a card has room
    /// for; the intent is a paragraph, and a paragraph in a title slot reads as
    /// a wall of text that hides the rest of the list.
    pub fn title(&self) -> &str {
        self.facts
            .title
            .as_deref()
            .unwrap_or_else(|| id::name(&self.id))
    }

    /// The project this task belongs to, as it was recorded when the task was
    /// founded.
    ///
    /// Recorded rather than looked up: a task carries the setup, repository, and
    /// caches it started with, so a task that travels or outlives an edit keeps
    /// the machine state it was asked for.
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

    /// How this task reads as an agent in a session title: `harness-model`.
    ///
    /// The model may carry a provider prefix; a title needs the model's own name.
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
