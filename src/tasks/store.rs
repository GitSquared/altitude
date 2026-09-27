use sqlx::{Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

use super::{
    Task, TaskAgentRun, TaskAgentTurnReport, TaskCurrentState, TaskError, TaskHistoryEvent,
    TaskOperatorNotice, TaskOperatorNoticeLevel,
};
use crate::{agent::AgentKind, db::Database};

#[derive(Clone)]
pub struct TaskStore {
    pool: SqlitePool,
}

impl TaskStore {
    pub fn new(database: &Database) -> Self {
        Self {
            pool: database.pool().clone(),
        }
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskError> {
        sqlx::query_as("SELECT tasks.id, tasks.agent_kind, task_statuses.* FROM tasks JOIN task_statuses ON task_statuses.task_id = tasks.id ORDER BY last_event_sequence DESC")
            .fetch_all(&self.pool).await.map_err(TaskError::Database)
    }

    pub async fn create_with_agent(
        &self,
        intent: String,
        agent_kind: Option<AgentKind>,
        agent_conversation_id: Option<String>,
    ) -> Result<Task, TaskError> {
        let mut task = Task {
            id: Uuid::new_v4().to_string(),
            agent_kind,
            current_state: TaskCurrentState::default(),
        };
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        sqlx::query("INSERT INTO tasks (id, agent_kind, created_at_ms) VALUES (?, ?, ?)")
            .bind(&task.id)
            .bind(agent_kind)
            .bind(now_ms())
            .execute(&mut *transaction)
            .await
            .map_err(TaskError::Database)?;
        Self::write_events(
            &mut transaction,
            &mut task,
            &[TaskHistoryEvent::IntentExpressed {
                intent,
                agent_conversation_id,
            }],
        )
        .await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    /// Serialize writers before reading the projection. Events and their projection commit together.
    pub async fn append_events(
        &self,
        task_id: &str,
        events: &[TaskHistoryEvent],
    ) -> Result<Task, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let mut task = sqlx::query_as::<_, Task>("SELECT tasks.id, tasks.agent_kind, task_statuses.* FROM tasks JOIN task_statuses ON task_statuses.task_id = tasks.id WHERE tasks.id = ?")
            .bind(task_id).fetch_one(&mut *transaction).await.map_err(TaskError::Database)?;
        Self::write_events(&mut transaction, &mut task, events).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    async fn write_events(
        transaction: &mut Transaction<'_, Sqlite>,
        task: &mut Task,
        events: &[TaskHistoryEvent],
    ) -> Result<(), TaskError> {
        let mut last_event_sequence = None;
        for event in events {
            let payload = serde_json::to_string(event).map_err(TaskError::HistoryData)?;
            let result = sqlx::query("INSERT INTO task_event_logs (id, task_id, recorded_at_ms, payload) VALUES (?, ?, ?, ?)")
                .bind(Uuid::new_v4().to_string()).bind(&task.id).bind(now_ms()).bind(payload)
                .execute(&mut **transaction).await.map_err(TaskError::Database)?;
            task.current_state.apply(event);
            last_event_sequence = Some(result.last_insert_rowid());
        }
        let Some(sequence) = last_event_sequence else {
            return Ok(());
        };
        let state = &task.current_state;
        sqlx::query("INSERT INTO task_statuses (task_id, intent, agent_conversation_id, execution_status, execution_pause_reason, execution_controller, operator_attention_level, sandbox_id, snapshot_path, last_error, latest_turn_summary, last_event_sequence) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(task_id) DO UPDATE SET intent = excluded.intent, agent_conversation_id = excluded.agent_conversation_id, execution_status = excluded.execution_status, execution_pause_reason = excluded.execution_pause_reason, execution_controller = excluded.execution_controller, operator_attention_level = excluded.operator_attention_level, sandbox_id = excluded.sandbox_id, snapshot_path = excluded.snapshot_path, last_error = excluded.last_error, latest_turn_summary = excluded.latest_turn_summary, last_event_sequence = excluded.last_event_sequence")
            .bind(&task.id).bind(&state.intent).bind(&state.agent_conversation_id)
            .bind(&state.execution_status).bind(&state.execution_pause_reason).bind(&state.execution_controller)
            .bind(&state.operator_attention_level).bind(&state.sandbox_id).bind(&state.snapshot_path)
            .bind(&state.last_error).bind(&state.latest_turn_summary).bind(sequence)
            .execute(&mut **transaction).await.map_err(TaskError::Database)?;
        Ok(())
    }

    pub async fn record_report(
        &self,
        task_id: &str,
        agent_run: &TaskAgentRun,
        report: &TaskAgentTurnReport,
    ) -> Result<(Task, Vec<TaskOperatorNotice>), TaskError> {
        let mut events = vec![TaskHistoryEvent::AgentTurnReported {
            agent_run_id: agent_run.id.clone(),
            requested_action: report.requested_action.clone(),
            operator_attention_level: report.operator_attention_level.clone(),
            summary: report.summary.clone(),
        }];
        let notices: Vec<_> = report
            .operator_notices
            .iter()
            .map(|draft| TaskOperatorNotice {
                id: Uuid::new_v4().to_string(),
                task_id: task_id.to_owned(),
                level: draft.level.clone(),
                title: draft.title.clone(),
                body: draft.body.clone(),
            })
            .collect();
        events.extend(
            notices
                .iter()
                .cloned()
                .map(|notice| TaskHistoryEvent::OperatorNoticeRaised { notice }),
        );
        Ok((self.append_events(task_id, &events).await?, notices))
    }

    pub async fn unacknowledged_operator_notices(
        &self,
        level: TaskOperatorNoticeLevel,
    ) -> Result<Vec<TaskOperatorNotice>, TaskError> {
        sqlx::query_as("SELECT id, task_id, level, title, body FROM task_operator_notices WHERE level = ? AND acknowledged = 0 ORDER BY sequence DESC")
            .bind(level).fetch_all(&self.pool).await.map_err(TaskError::Database)
    }

    pub async fn acknowledge_operator_notice(&self, notice_id: &str) -> Result<(), TaskError> {
        let task_id: Option<String> = sqlx::query_scalar(
            "SELECT task_id FROM task_operator_notices WHERE id = ? AND acknowledged = 0",
        )
        .bind(notice_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(TaskError::Database)?;
        if let Some(task_id) = task_id {
            self.append_events(
                &task_id,
                &[TaskHistoryEvent::OperatorNoticeAcknowledged {
                    notice_id: notice_id.to_owned(),
                }],
            )
            .await?;
        }
        Ok(())
    }

    pub async fn delete(&self, task_id: &str) -> Result<(), TaskError> {
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(TaskError::Database)?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
