use chrono::{Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Type};
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
    pub level: TaskOperatorNoticeLevel,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskOperatorNoticeLevel {
    Info,
    Attention,
    Alarm,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskOperatorAttentionLevel {
    Nominal,
    Requested,
    Alarm,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionPauseReason {
    AwaitingOperator,
    ApplicationShutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum TaskExecutionController {
    Altitude,
    Operator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskAgentRunMode {
    Autopilot,
    AutopilotReport,
    Operator,
}

#[derive(Clone, Debug)]
pub struct TaskAgentRun {
    pub id: String,
    pub task_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, FromRow)]
pub struct TaskOperatorNotice {
    pub id: String,
    pub task_id: String,
    pub level: TaskOperatorNoticeLevel,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, FromRow)]
pub struct Task {
    pub id: String,
    pub agent_kind: Option<AgentKind>,
    #[sqlx(flatten)]
    pub current_state: TaskCurrentState,
}

/// A cache of history-derived state, never an independent source of truth.
#[derive(Clone, Debug, FromRow)]
pub struct TaskCurrentState {
    pub intent: String,
    pub agent_conversation_id: Option<String>,
    pub execution_status: TaskExecutionStatus,
    pub execution_pause_reason: Option<TaskExecutionPauseReason>,
    pub execution_controller: TaskExecutionController,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub sandbox_id: Option<String>,
    pub snapshot_path: Option<String>,
    pub last_error: Option<String>,
    pub latest_turn_summary: Option<String>,
}

impl Default for TaskCurrentState {
    fn default() -> Self {
        Self {
            intent: String::new(),
            agent_conversation_id: None,
            execution_status: TaskExecutionStatus::Pending,
            execution_pause_reason: None,
            execution_controller: TaskExecutionController::Altitude,
            operator_attention_level: TaskOperatorAttentionLevel::Nominal,
            sandbox_id: None,
            snapshot_path: None,
            last_error: None,
            latest_turn_summary: None,
        }
    }
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
        controller: TaskExecutionController,
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

impl TaskCurrentState {
    pub(super) fn apply(&mut self, event: &TaskHistoryEvent) {
        match event {
            TaskHistoryEvent::IntentExpressed {
                intent,
                agent_conversation_id,
            } => {
                self.intent = intent.clone();
                self.agent_conversation_id = agent_conversation_id.clone();
            }
            TaskHistoryEvent::AgentRunStarted {
                sandbox_id,
                controller,
                ..
            } => {
                self.execution_status = TaskExecutionStatus::Running;
                self.execution_pause_reason = None;
                self.execution_controller = controller.clone();
                self.sandbox_id = Some(sandbox_id.clone());
                self.last_error = None;
            }
            TaskHistoryEvent::SandboxProvisioned { sandbox_id } => {
                self.sandbox_id = Some(sandbox_id.clone());
            }
            TaskHistoryEvent::AgentTurnReported {
                operator_attention_level,
                summary,
                ..
            } => {
                if let Some(level) = operator_attention_level {
                    self.operator_attention_level = level.clone();
                }
                self.latest_turn_summary = Some(summary.clone());
            }
            TaskHistoryEvent::ExecutionPaused {
                reason,
                snapshot_path,
            } => {
                self.execution_status = TaskExecutionStatus::Paused;
                self.execution_pause_reason = Some(reason.clone());
                self.sandbox_id = None;
                self.snapshot_path = Some(snapshot_path.clone());
                self.last_error = None;
            }
            TaskHistoryEvent::ExecutionRecovered { snapshot_path } => {
                self.execution_status = TaskExecutionStatus::Pending;
                self.execution_pause_reason = None;
                self.sandbox_id = None;
                self.snapshot_path = Some(snapshot_path.clone());
                self.last_error = None;
            }
            TaskHistoryEvent::TaskSnapshotSaved { snapshot_path } => {
                self.sandbox_id = None;
                self.snapshot_path = Some(snapshot_path.clone());
            }
            TaskHistoryEvent::ExecutionFailed { error } => {
                self.execution_status = TaskExecutionStatus::Failed;
                self.execution_pause_reason = None;
                self.last_error = Some(error.clone());
            }
            TaskHistoryEvent::TaskCompleted => {
                self.execution_status = TaskExecutionStatus::Completed;
                self.execution_pause_reason = None;
                self.sandbox_id = None;
                self.snapshot_path = None;
                self.last_error = None;
            }
            TaskHistoryEvent::AgentRunEnded { .. }
            | TaskHistoryEvent::OperatorNoticeRaised { .. }
            | TaskHistoryEvent::OperatorNoticeAcknowledged { .. } => {}
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskAgentTurnContext {
    pub execution_status: TaskExecutionStatus,
    pub execution_controller: TaskExecutionController,
    pub operator_attention_level: TaskOperatorAttentionLevel,
    pub datetime: String,
}

impl fmt::Display for TaskAgentTurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "execution_status={:?}; execution_controller={:?}; operator_attention_level={:?}; current_datetime={}",
            self.execution_status,
            self.execution_controller,
            self.operator_attention_level,
            self.datetime
        )
    }
}

impl Task {
    pub fn autopilot_context(&self) -> TaskAgentTurnContext {
        TaskAgentTurnContext {
            execution_status: TaskExecutionStatus::Running,
            execution_controller: TaskExecutionController::Altitude,
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
