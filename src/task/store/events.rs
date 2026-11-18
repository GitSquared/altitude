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

    // A run's ending is a fact about one attempt. It is accepted only while the run
    // is the task's newest and has no ending of its own recorded yet, read and
    // written under one lock: a callback that arrives after a later attempt began,
    // or a replay of an ending already consumed, must not pause, fail or continue a
    // task it no longer owns.
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
        if !run_can_settle(&mut transaction, task_id, agent_run_id).await? {
            transaction.rollback().await.map_err(TaskError::Database)?;
            return Ok(None);
        }
        Self::write_events(&mut transaction, task_id, events).await?;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(Some(task))
    }

    // The same predicate as the write, for a caller that only wants to know before
    // touching a sandbox: a duplicate or superseded ending can be dropped here
    // without suspending or releasing anything.
    pub async fn run_can_settle(
        &self,
        task_id: &str,
        agent_run_id: &str,
    ) -> Result<bool, TaskError> {
        let mut transaction = self.pool.begin().await.map_err(TaskError::Database)?;
        let can_settle = run_can_settle(&mut transaction, task_id, agent_run_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(can_settle)
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

// The predicate an outcome write consumes and a preflight reads: the task's newest
// started run is this one, and it carries no AgentRunEnded yet. The newest run is
// read from the same events that record it, and the ended marker is the outcome
// itself, so nothing new has to be kept in step.
async fn run_can_settle(
    transaction: &mut Transaction<'_, Sqlite>,
    task_id: &str,
    agent_run_id: &str,
) -> Result<bool, TaskError> {
    let latest: Option<String> = sqlx::query_scalar(
        r#"
        SELECT payload ->> '$.data.agent_run_id'
        FROM task_event_logs
        WHERE task_id = ?1 AND event_type = 'agent_run_started'
        ORDER BY sequence DESC LIMIT 1
        "#,
    )
    .bind(task_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(TaskError::Database)?;
    if latest.as_deref() != Some(agent_run_id) {
        return Ok(false);
    }
    let already_ended: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT 1
        FROM task_event_logs
        WHERE task_id = ?1
          AND event_type = 'agent_run_ended'
          AND payload ->> '$.data.agent_run_id' = ?2
        LIMIT 1
        "#,
    )
    .bind(task_id)
    .bind(agent_run_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(TaskError::Database)?;
    Ok(already_ended.is_none())
}
