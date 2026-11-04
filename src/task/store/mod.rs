
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction, types::Json};

use crate::id;
use crate::project::Project;
use crate::task::{
    Task, TaskError,
    TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus, TaskFacts, TaskHistoryEvent, TaskOperatorAttentionLevel, TaskOperatorSignalDraft,
};
use crate::{
    agent::AgentKind,
    db::Database,
    tracker::IssueReference,
    vcs::{CodeContribution, ContributionObservation},
};

mod events;
mod reports;
mod signals;

// SQL returns the latest recorded properties without interpreting task lifecycle.
#[derive(FromRow)]
struct TaskRow {
    id: String,
    agent_kind: Option<AgentKind>,
    host: String,
    contributions: Json<Vec<CodeContribution>>,
    observations: Json<Vec<ContributionObservation>>,
    #[sqlx(flatten)]
    facts: TaskFacts,
}

impl From<TaskRow> for Task {
    fn from(row: TaskRow) -> Self {
        Self {
            id: row.id,
            agent_kind: row.agent_kind,
            host: row.host,
            code_contributions: row.contributions.0,
            code_contribution_observations: row.observations.0,
            facts: row.facts,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskDraft {
    pub intent: String,
    pub project: Project,
    pub identity_id: String,
    pub identity_label: String,
    pub host: String,
    pub agent_kind: Option<AgentKind>,
    pub agent_conversation_id: Option<String>,
}

#[derive(Clone)]
pub struct TaskStore {
    pool: SqlitePool,
}

pub(crate) enum ReviewRequest<'a> {
    Raise,
    Hold(&'a TaskOperatorSignalDraft),
}

impl TaskStore {
    pub fn new(database: &Database) -> Self {
        Self {
            pool: database.pool().clone(),
        }
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskError> {
        sqlx::query_as::<_, TaskRow>(concat!(
            include_str!("read_task.sql"),
            " WHERE tasks.removed_at_ms IS NULL ORDER BY last_event_sequence DESC"
        ))
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(Task::from).collect())
        .map_err(TaskError::Database)
    }

    pub async fn list_held_reviews(&self) -> Result<Vec<Task>, TaskError> {
        sqlx::query_as::<_, TaskRow>(concat!(
            include_str!("read_task.sql"),
            " WHERE tasks.removed_at_ms IS NULL",
            " AND tasks.id IN (SELECT task_id FROM task_deferred_reviews)",
            " ORDER BY last_event_sequence DESC"
        ))
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(Task::from).collect())
        .map_err(TaskError::Database)
    }

    pub async fn task_for_signal(
        &self,
        task_id: &str,
        signal_id: &str,
    ) -> Result<Option<Task>, TaskError> {
        let known: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM task_operator_signals WHERE task_id = ?1 AND id = ?2",
        )
        .bind(task_id)
        .bind(signal_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(TaskError::Database)?;
        match known {
            Some(_) => self.get(task_id).await,
            None => Ok(None),
        }
    }

    pub async fn get(&self, task_id: &str) -> Result<Option<Task>, TaskError> {
        sqlx::query_as::<_, TaskRow>(concat!(
            include_str!("read_task.sql"),
            " WHERE tasks.id = ?"
        ))
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(Task::from))
        .map_err(TaskError::Database)
    }

    pub async fn create(
        &self,
        draft: TaskDraft,
        history: Option<&[(i64, TaskHistoryEvent)]>,
        arrival: &[TaskHistoryEvent],
    ) -> Result<Task, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        // Binding is idempotent: if an active task already holds this issue, that
        // task is the answer, and founding a second one would double the work.
        // Read under the same write lock that would have created the second task.
        if let Some(reference) = tracker_binding(arrival)
            && let Some(existing) = Self::active_bound_task(&mut transaction, reference).await?
        {
            transaction.commit().await.map_err(TaskError::Database)?;
            return Ok(existing);
        }
        let TaskDraft {
            intent,
            project,
            identity_id,
            identity_label,
            host,
            agent_kind,
            agent_conversation_id,
        } = draft;
        let task_id = loop {
            let candidate = id::task();
            let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM tasks WHERE id = ?")
                .bind(&candidate)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(TaskError::Database)?;
            if exists.is_none() {
                break candidate;
            }
        };
        sqlx::query(
            r#"
            INSERT INTO tasks (id, agent_kind, host, created_at_ms)
            VALUES (?1, ?2, ?3, ?4)
        "#,
        )
        .bind(&task_id)
        .bind(agent_kind)
        .bind(host)
        .bind(now_ms())
        .execute(&mut *transaction)
        .await
        .map_err(TaskError::Database)?;
        match history {
            Some(history) => {
                Self::write_recorded_events(&mut transaction, &task_id, history).await?;
            }
            None => {
                Self::write_events(
                    &mut transaction,
                    &task_id,
                    &[TaskHistoryEvent::IntentExpressed {
                        execution_status: TaskExecutionStatus::Pending,
                        execution_mode: TaskExecutionMode::Autopilot,
                        operator_attention_level: TaskOperatorAttentionLevel::Nominal,
                        sandbox_available: false,
                        snapshot_available: false,
                        intent,
                        project,
                        identity_id,
                        identity_label,
                        agent_conversation_id,
                    }],
                )
                .await?;
            }
        }
        Self::write_events(&mut transaction, &task_id, arrival).await?;
        let task = Self::read_task(&mut transaction, &task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    async fn active_bound_task(
        transaction: &mut Transaction<'_, Sqlite>,
        reference: &IssueReference,
    ) -> Result<Option<Task>, TaskError> {
        let provider = match serde_json::to_value(reference.provider) {
            Ok(serde_json::Value::String(provider)) => provider,
            Ok(_) => unreachable!("a tracker kind serializes to a string"),
            Err(error) => return Err(TaskError::HistoryData(error)),
        };
        // The reasons that mean "the tracker ended this" are a rule about a task, not
        // about a query: the newest candidates come back and the rule leaves out the
        // ones that are records rather than work, so one list serves every caller.
        let rows = sqlx::query_as::<_, TaskRow>(concat!(
            include_str!("read_task.sql"),
            r#" WHERE tasks.removed_at_ms IS NULL
                  AND NOT EXISTS (
                      SELECT 1 FROM task_event_logs AS finished
                      WHERE finished.task_id = tasks.id
                        AND finished.event_type IN ('operator_signal_confirmed', 'task_completed')
                  )
                  AND EXISTS (
                      SELECT 1 FROM task_event_logs AS bound
                      WHERE bound.task_id = tasks.id
                        AND bound.event_type = 'tracker_issue_bound'
                        AND bound.payload ->> '$.data.issue.issue.reference.provider' = ?1
                        AND bound.payload ->> '$.data.issue.issue.reference.workspace_id' = ?2
                        AND bound.payload ->> '$.data.issue.issue.reference.id' = ?3
                  )
                ORDER BY tasks.created_at_ms DESC
                LIMIT 5"#
        ))
        .bind(provider)
        .bind(&reference.workspace_id)
        .bind(&reference.id)
        .fetch_all(&mut **transaction)
        .await
        .map_err(TaskError::Database)?;
        Ok(rows.into_iter().map(Task::from).find(|task| {
            !task
                .facts
                .execution_pause_reason
                .as_ref()
                .is_some_and(TaskExecutionPauseReason::is_out_of_play)
        }))
    }

    pub async fn is_kept(&self, task_id: &str) -> Result<bool, TaskError> {
        let active: Option<bool> =
            sqlx::query_scalar("SELECT removed_at_ms IS NULL FROM tasks WHERE id = ?")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(TaskError::Database)?;
        Ok(active.unwrap_or(false))
    }

    async fn read_task(
        transaction: &mut Transaction<'_, Sqlite>,
        task_id: &str,
    ) -> Result<Task, TaskError> {
        sqlx::query_as::<_, TaskRow>(concat!(
            include_str!("read_task.sql"),
            " WHERE tasks.id = ?"
        ))
        .bind(task_id)
        .fetch_one(&mut **transaction)
        .await
        .map_err(TaskError::Database)
        .map(Task::from)
    }

    pub async fn request_control(
        &self,
        task_id: &str,
        requested_mode: TaskExecutionMode,
        agent_run_id: &str,
    ) -> Result<Task, TaskError> {
        self.append_events(
            task_id,
            &[TaskHistoryEvent::ControlRequested {
                requested_mode,
                agent_run_id: agent_run_id.to_owned(),
            }],
        )
        .await
    }

    pub async fn relinquish(
        &self,
        task_id: &str,
        transfer_id: &str,
        recipient: &str,
    ) -> Result<(), TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        Self::write_events(
            &mut transaction,
            task_id,
            &[TaskHistoryEvent::TransferCommitted {
                transfer_id: transfer_id.to_owned(),
                recipient: recipient.to_owned(),
            }],
        )
        .await?;
        sqlx::query("UPDATE tasks SET removed_at_ms = ? WHERE id = ?")
            .bind(now_ms())
            .bind(task_id)
            .execute(&mut *transaction)
            .await
            .map_err(TaskError::Database)?;
        transaction.commit().await.map_err(TaskError::Database)
    }

    pub async fn forget(&self, task_id: &str) -> Result<(), TaskError> {
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(TaskError::Database)?;
        Ok(())
    }

    pub async fn remove(&self, task_id: &str) -> Result<(), TaskError> {
        sqlx::query("UPDATE tasks SET removed_at_ms = ? WHERE id = ?")
            .bind(now_ms())
            .bind(task_id)
            .execute(&self.pool)
            .await
            .map_err(TaskError::Database)?;
        Ok(())
    }
}

fn tracker_binding(events: &[TaskHistoryEvent]) -> Option<&IssueReference> {
    events.iter().find_map(|event| match event {
        TaskHistoryEvent::TrackerIssueBound { issue, .. } => Some(&issue.issue.reference),
        _ => None,
    })
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
