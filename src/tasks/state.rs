use chrono::{Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::agent::AgentKind;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskAgentTurnReport {
    pub requested_action: TaskAgentRequestedAction,
    pub operator_attention_level: Option<TaskOperatorAttentionLevel>,
    pub summary: String,
    #[serde(default)]
    pub operator_notices: Vec<TaskOperatorNoticeDraft>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskAgentRequestedAction {
    Continue,
    AwaitOperator,
    Complete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOperatorNoticeDraft {
    pub level: TaskOperatorAttentionLevel,
    pub title: String,
    pub body: String,
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
pub struct TaskOperatorNotice {
    pub id: String,
    pub task_id: String,
    pub level: TaskOperatorAttentionLevel,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub agent_kind: Option<AgentKind>,
    pub current_state: TaskCurrentState,
}

/// Derived directly from task history by the task_statuses view.
#[derive(Clone, Debug, Deserialize)]
pub struct TaskCurrentState {
    pub intent: String,
    pub agent_conversation_id: Option<String>,
    pub execution_status: TaskExecutionStatus,
    pub execution_pause_reason: Option<TaskExecutionPauseReason>,
    pub execution_mode: TaskExecutionMode,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub sandbox_id: Option<String>,
    pub snapshot_path: Option<String>,
    pub last_error: Option<String>,
    pub latest_turn_summary: Option<String>,
}

/// Semantic history written by Altitude's controllers, never directly by agents.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TaskHistoryEvent {
    IntentExpressed {
        intent: String,
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
        operator_attention_level: Option<TaskOperatorAttentionLevel>,
        summary: String,
    },
    OperatorNoticeRaised {
        notice: TaskOperatorNotice,
    },
    OperatorNoticeAcknowledged {
        notice_id: String,
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
}

impl fmt::Display for TaskAgentTurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "execution_status={:?}; execution_mode={:?}; operator_attention_level={:?}; current_datetime={}",
            self.execution_status,
            self.execution_mode,
            self.operator_attention_level,
            self.datetime
        )
    }
}

impl Task {
    pub fn turn_context(&self, execution_mode: TaskExecutionMode) -> TaskAgentTurnContext {
        TaskAgentTurnContext {
            execution_status: TaskExecutionStatus::Running,
            execution_mode,
            operator_attention_level: self.current_state.operator_attention_level.clone(),
            datetime: Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
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
