use chrono::{Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::agent::AgentKind;
use crate::isolation::Environment;
use crate::vcs::{CodeContribution, ContributionObservation};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskAgentTurnReport {
    pub requested_action: TaskAgentRequestedAction,
    pub summary: String,
    #[serde(default)]
    pub operator_signal: Option<TaskOperatorSignalDraft>,
    #[serde(default)]
    pub code_contribution_urls: Vec<String>,
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
    pub priority: TaskOperatorSignalPriority,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskOperatorSignalKind {
    Update,
    Decision,
    /// The work is finished as far as the harness can tell, and the operator
    /// decides whether it is done. Confirming closes the task and disposes of its
    /// capsule; replying sends it back for more work.
    InReview,
    /// The harness stopped; the operator must repair the problem before retrying.
    Failure,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOperatorSignalPriority {
    Low,
    Normal,
    High,
    Urgent,
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorSignal {
    pub id: String,
    pub task_id: String,
    pub kind: TaskOperatorSignalKind,
    pub priority: TaskOperatorSignalPriority,
    pub title: String,
    pub body: String,
    pub raised_at_ms: i64,
    pub read: bool,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub agent_kind: Option<AgentKind>,
    /// The machine that holds this task's capsule, which is the machine that may run
    /// it. Its own machine until the task is delegated.
    pub host: String,
    pub code_contributions: Vec<CodeContribution>,
    /// Latest observed state per contribution, oldest contribution first.
    pub code_contribution_observations: Vec<ContributionObservation>,
    pub facts: TaskFacts,
}

/// Latest recorded values. Consumers decide which facts are relevant to their operation.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskFacts {
    pub intent: String,
    /// The short name the agent chose for the task, when it has chosen one.
    pub title: Option<String>,
    pub environment_name: String,
    pub environment_setup_command: String,
    pub agent_conversation_id: Option<String>,
    pub execution_status: TaskExecutionStatus,
    pub execution_pause_reason: Option<TaskExecutionPauseReason>,
    pub execution_mode: TaskExecutionMode,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub sandbox_id: Option<String>,
    pub sandbox_available: bool,
    pub snapshot_path: Option<String>,
    pub snapshot_available: bool,
    pub last_error: Option<String>,
    pub latest_turn_summary: Option<String>,
    #[sqlx(json(nullable))]
    pub latest_operator_answer: Option<TaskOperatorAnswer>,
    /// The blocking signal the operator is being waited on for, when one is open.
    pub awaiting_signal_kind: Option<TaskOperatorSignalKind>,
    pub unread_operator_signal_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorAnswer {
    pub signal_id: String,
    pub answer: String,
}

impl fmt::Display for TaskOperatorAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Preserve escaping when embedding an operator's answer in agent context.
        let json = serde_json::to_string(self).map_err(|_| fmt::Error)?;
        formatter.write_str(&json)
    }
}

/// Semantic history written by Altitude's controllers, never directly by agents.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
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
        environment: Environment,
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
    },
    /// The operator asked for the task to be driven by someone else.
    ///
    /// It names the run it was asked during. Only that run can act on it, so a
    /// request cannot land on whichever run happens to end next; a request with
    /// no run to receive it is refused rather than recorded.
    ControlRequested {
        /// The driver the operator asked for, not the one in effect.
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
        summary: String,
    },
    OperatorSignalRaised {
        signal: TaskOperatorSignal,
    },
    OperatorSignalRead {
        signal_id: String,
    },
    OperatorSignalAcknowledged {
        signal_id: String,
    },
    OperatorSignalAnswered {
        signal_id: String,
        answer: String,
    },
    /// The operator accepted a wrap-up request, which is the only way a task ends.
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAgentRunOutcome {
    Continued,
    AwaitingOperator,
    Failed,
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
    /// The machine this task's capsule runs on.
    pub host: String,
    pub environment: String,
    pub agent: Option<AgentKind>,
    pub execution_status: TaskExecutionStatus,
    pub execution_mode: TaskExecutionMode,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub awaiting_signal_kind: Option<TaskOperatorSignalKind>,
    pub unread_operator_signal_count: u32,
    pub last_error: Option<String>,
    pub datetime: String,
    pub latest_operator_answer: Option<TaskOperatorAnswer>,
    /// What the linked contributions currently look like outside Altitude.
    pub code_contribution_observations: Vec<ContributionObservation>,
}

impl fmt::Display for TaskAgentTurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "task_id={}; title={:?}; harness={}; host={}; environment={}",
            self.task_id,
            self.title,
            self.agent
                .map(|kind| kind.to_string())
                .unwrap_or_else(|| "none".into()),
            self.host,
            self.environment,
        )?;
        writeln!(
            formatter,
            "execution_status={:?}; execution_mode={:?}; attention={:?}; \
             awaiting_signal={:?}; unread_operator_signals={}; last_error={}",
            self.execution_status,
            self.execution_mode,
            self.operator_attention_level,
            self.awaiting_signal_kind,
            self.unread_operator_signal_count,
            self.last_error.as_deref().unwrap_or("none"),
        )?;
        write!(formatter, "current_datetime={}", self.datetime)?;
        if let Some(answer) = &self.latest_operator_answer {
            write!(formatter, "\nlatest_operator_answer={answer}")?;
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

/// The longest a task title may be before it stops being readable in a list.
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
    /// What a list calls this task: the agent's title, or what was asked for
    /// before the agent has chosen one.
    pub fn title(&self) -> &str {
        self.facts.title.as_deref().unwrap_or(&self.facts.intent)
    }

    /// The environment this task runs in.
    ///
    /// The agent it runs under is recorded separately, as the task's agent kind.
    pub fn environment(&self) -> Environment {
        Environment {
            name: self.facts.environment_name.clone(),
            setup_command: self.facts.environment_setup_command.clone(),
        }
    }

    pub fn turn_context(&self, execution_mode: TaskExecutionMode) -> TaskAgentTurnContext {
        TaskAgentTurnContext {
            task_id: self.id.clone(),
            title: self.title().to_owned(),
            host: self.host.clone(),
            environment: self.facts.environment_name.clone(),
            agent: self.agent_kind,
            execution_status: TaskExecutionStatus::Running,
            execution_mode,
            operator_attention_level: self.facts.operator_attention_level.clone(),
            awaiting_signal_kind: self.facts.awaiting_signal_kind,
            unread_operator_signal_count: self.facts.unread_operator_signal_count,
            last_error: self.facts.last_error.clone(),
            datetime: Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            latest_operator_answer: self.facts.latest_operator_answer.clone(),
            code_contribution_observations: self.code_contribution_observations.clone(),
        }
    }
}
