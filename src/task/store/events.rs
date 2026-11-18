use super::*;

use sqlx::{Sqlite, Transaction};
use uuid::Uuid;

use crate::task::{Task, TaskError, TaskHistoryEvent};

impl TaskStore {
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
        Self::write_events(&mut transaction, task_id, events).await?;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    // A run's ending is a fact about one attempt, and the callback that carries it
    // can arrive after the task has begun another. The write is accepted only while
    // the task still names this run as its newest, read under the same write lock:
    // a late ending must not pause, fail, or continue a task a later attempt owns.
    pub async fn record_run_outcome(
        &self,
        task_id: &str,
        agent_run_id: &str,
        events: &[TaskHistoryEvent],
    ) -> Result<Option<Task>, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let current: Option<String> = sqlx::query_scalar(
            r#"
            SELECT payload ->> '$.data.agent_run_id'
            FROM task_event_logs
            WHERE task_id = ?1 AND event_type = 'agent_run_started'
            ORDER BY sequence DESC LIMIT 1
            "#,
        )
        .bind(task_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TaskError::Database)?;
        if current.as_deref() != Some(agent_run_id) {
            transaction.rollback().await.map_err(TaskError::Database)?;
            return Ok(None);
        }
        Self::write_events(&mut transaction, task_id, events).await?;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(Some(task))
    }

    pub async fn events(&self, task_id: &str) -> Result<Vec<(i64, TaskHistoryEvent)>, TaskError> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT recorded_at_ms, payload FROM task_event_logs WHERE task_id = ? ORDER BY sequence",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .map_err(TaskError::Database)?;
        rows.into_iter()
            .map(|(recorded_at_ms, payload)| {
                serde_json::from_str(&payload)
                    .map(|event| (recorded_at_ms, event))
                    .map_err(TaskError::HistoryData)
            })
            .collect()
    }

    pub(super) async fn write_recorded_events(
        transaction: &mut Transaction<'_, Sqlite>,
        task_id: &str,
        events: &[(i64, TaskHistoryEvent)],
    ) -> Result<(), TaskError> {
        for (recorded_at_ms, event) in events {
            let payload = serde_json::to_string(event).map_err(TaskError::HistoryData)?;
            sqlx::query(
                r#"
                INSERT INTO task_event_logs (id, task_id, recorded_at_ms, payload)
                VALUES (?1, ?2, ?3, ?4)
            "#,
            )
            .bind(Uuid::new_v4().to_string())
            .bind(task_id)
            .bind(recorded_at_ms)
            .bind(payload)
            .execute(&mut **transaction)
            .await
            .map_err(TaskError::Database)?;
        }
        Ok(())
    }

    pub(super) async fn write_events(
        transaction: &mut Transaction<'_, Sqlite>,
        task_id: &str,
        events: &[TaskHistoryEvent],
    ) -> Result<(), TaskError> {
        for event in events {
            let payload = serde_json::to_string(event).map_err(TaskError::HistoryData)?;
            sqlx::query(
                r#"
                INSERT INTO task_event_logs (id, task_id, recorded_at_ms, payload)
                VALUES (?1, ?2, ?3, ?4)
            "#,
            )
            .bind(Uuid::new_v4().to_string())
            .bind(task_id)
            .bind(now_ms())
            .bind(payload)
            .execute(&mut **transaction)
            .await
            .map_err(TaskError::Database)?;
        }
        Ok(())
    }
}
