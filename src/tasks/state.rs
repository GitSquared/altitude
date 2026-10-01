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
pub enum TaskAgentRequestedAction {
    Continue,
    AwaitOperator,
    Complete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorSignalDraft {
    pub kind: TaskOperatorSignalKind,
    pub priority: TaskOperatorSignalPriority,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOperatorSignalKind {
    Update,
    Decision,
    Review,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOperatorSignalPriority {
    Low,
    Normal,
    High,
    Urgent,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOperatorAttentionLevel {
    Nominal,
    Requested,
    Alarm,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskExecutionStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskExecutionPauseReason {
    AwaitingOperator,
    ApplicationShutdown,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
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
    pub current_state: TaskCurrentState,
}

/// Derived directly from task history by the task_statuses view.
#[derive(Clone, Debug, Deserialize)]
pub struct TaskCurrentState {
    pub intent: String,
    pub environment_name: String,
    pub environment_setup_command: String,
    pub agent_conversation_id: Option<String>,
    pub execution_status: TaskExecutionStatus,
    pub execution_pause_reason: Option<TaskExecutionPauseReason>,
    pub execution_mode: TaskExecutionMode,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub sandbox_id: Option<String>,
    pub snapshot_path: Option<String>,
    pub last_error: Option<String>,
    pub latest_turn_summary: Option<String>,
    pub latest_operator_answer: Option<serde_json::Value>,
    pub unread_operator_signal_count: u32,
}

/// Semantic history written by Altitude's controllers, never directly by agents.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TaskHistoryEvent {
    IntentExpressed {
        intent: String,
        environment: DevelopmentEnvironment,
        agent_conversation_id: Option<String>,
    },
    SandboxProvisioned {
        sandbox_id: String,
    },
    AgentRunStarted {
        agent_run_id: String,
        sandbox_id: String,
        mode: TaskExecutionMode,
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
        reason: TaskExecutionPauseReason,
        snapshot_path: String,
    },
    ExecutionRecovered {
        snapshot_path: String,
    },
    TaskSnapshotSaved {
        snapshot_path: String,
    },
    ExecutionFailed {
        error: String,
    },
    TaskCompleted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAgentRunOutcome {
    Continued,
    AwaitingOperator,
    Completed,
    Failed,
}

#[derive(Clone, Debug)]
pub struct TaskAgentTurnContext {
    pub execution_status: TaskExecutionStatus,
    pub execution_mode: TaskExecutionMode,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub datetime: String,
    pub latest_operator_answer: Option<serde_json::Value>,
    /// What the linked contributions currently look like outside Altitude.
    pub code_contribution_observations: Vec<ContributionObservation>,
}

impl fmt::Display for TaskAgentTurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "execution_status={:?}; execution_mode={:?}; operator_attention_level={:?}; current_datetime={}; latest_operator_answer={}",
            self.execution_status,
            self.execution_mode,
            self.operator_attention_level,
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
    pub fn development_environment(&self) -> DevelopmentEnvironment {
        DevelopmentEnvironment {
            name: self.current_state.environment_name.clone(),
            setup_command: self.current_state.environment_setup_command.clone(),
        }
    }

    pub fn turn_context(&self, execution_mode: TaskExecutionMode) -> TaskAgentTurnContext {
        TaskAgentTurnContext {
            execution_status: TaskExecutionStatus::Running,
            execution_mode,
            operator_attention_level: self.current_state.operator_attention_level.clone(),
            datetime: Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            latest_operator_answer: self.current_state.latest_operator_answer.clone(),
            code_contribution_observations: self.code_contribution_observations.clone(),
        }
    }

    pub fn should_resume(&self) -> bool {
        matches!(
            self.current_state.execution_status,
            TaskExecutionStatus::Pending | TaskExecutionStatus::Running
        ) || (self.current_state.execution_status == TaskExecutionStatus::Paused
            && self.current_state.execution_pause_reason
                == Some(TaskExecutionPauseReason::ApplicationShutdown))
    }

    pub fn should_suspend(&self) -> bool {
        self.current_state.sandbox_id.is_some()
            && matches!(
                self.current_state.execution_status,
                TaskExecutionStatus::Running | TaskExecutionStatus::Failed
            )
    }
}
