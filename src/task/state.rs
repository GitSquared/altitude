use chrono::{Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::agent::AgentKind;
use crate::development::DevelopmentEnvironment;
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
    pub code_contributions: Vec<CodeContribution>,
    /// Latest observed state per contribution, oldest contribution first.
    pub code_contribution_observations: Vec<ContributionObservation>,
    pub facts: TaskFacts,
}

/// Latest recorded values. Consumers decide which facts are relevant to their operation.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct TaskFacts {
    pub intent: String,
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
    IntentExpressed {
        execution_status: TaskExecutionStatus,
        execution_mode: TaskExecutionMode,
        operator_attention_level: TaskOperatorAttentionLevel,
        sandbox_available: bool,
        snapshot_available: bool,
        intent: String,
        environment: DevelopmentEnvironment,
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

#[derive(Clone, Debug)]
pub struct TaskAgentTurnContext {
    pub execution_status: TaskExecutionStatus,
    pub execution_mode: TaskExecutionMode,
    pub datetime: String,
    pub latest_operator_answer: Option<TaskOperatorAnswer>,
    /// What the linked contributions currently look like outside Altitude.
    pub code_contribution_observations: Vec<ContributionObservation>,
}

impl fmt::Display for TaskAgentTurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "execution_status={:?}; execution_mode={:?}; current_datetime={}; latest_operator_answer={}",
            self.execution_status,
            self.execution_mode,
            self.datetime,
            self.latest_operator_answer
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "none".into())
        )?;
        if self.code_contribution_observations.is_empty() {
            return Ok(());
        }
        write!(formatter, "; code_contributions=[")?;
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
        write!(formatter, "]")
    }
}

impl Task {
    /// The environment this task runs in.
    ///
    /// The agent it runs under is recorded separately, as the task's agent kind.
    pub fn development_environment(&self) -> DevelopmentEnvironment {
        DevelopmentEnvironment {
            name: self.facts.environment_name.clone(),
            setup_command: self.facts.environment_setup_command.clone(),
        }
    }

    pub fn turn_context(&self, execution_mode: TaskExecutionMode) -> TaskAgentTurnContext {
        TaskAgentTurnContext {
            execution_status: TaskExecutionStatus::Running,
            execution_mode,
            datetime: Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            latest_operator_answer: self.facts.latest_operator_answer.clone(),
            code_contribution_observations: self.code_contribution_observations.clone(),
        }
    }
}
