use super::*;

use sqlx::types::Json;

use crate::task::{Task, TaskError, TaskHistoryEvent};
use crate::vcs::{CodeContribution, ContributionObservation};

impl TaskStore {
    pub async fn link_code_contribution(
        &self,
        task_id: &str,
        contribution: &CodeContribution,
    ) -> Result<(Task, bool), TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let event = TaskHistoryEvent::CodeContributionLinked {
            contribution: contribution.clone(),
        };
        let payload = serde_json::to_string(&event).map_err(TaskError::HistoryData)?;
        let inserted = sqlx::query(
            r#"
            INSERT INTO task_event_logs (id, task_id, recorded_at_ms, payload)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(id) DO NOTHING
        "#,
        )
        .bind(format!("code-contribution:{task_id}:{}", contribution.id))
        .bind(task_id)
        .bind(now_ms())
        .bind(payload)
        .execute(&mut *transaction)
        .await
        .map_err(TaskError::Database)?
        .rows_affected()
            > 0;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok((task, inserted))
    }

    pub async fn record_code_contribution_observations(
        &self,
        task_id: &str,
        observations: &[ContributionObservation],
    ) -> Result<(Task, bool), TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let mut moved = false;
        for observation in observations {
            let previous: Option<Json<ContributionObservation>> = sqlx::query_scalar(
                r#"
                SELECT observation
                FROM task_code_contribution_observations
                WHERE task_id = ?1
                    AND json_extract(observation, '$.contribution_id') = ?2
                ORDER BY sequence DESC
                LIMIT 1
                "#,
            )
            .bind(task_id)
            .bind(&observation.contribution_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(TaskError::Database)?;
            if previous
                .as_ref()
                .is_some_and(|previous| previous.0 == *observation)
            {
                continue;
            }
            moved = true;
            Self::write_events(
                &mut transaction,
                task_id,
                &[TaskHistoryEvent::CodeContributionObserved {
                    observation: observation.clone(),
                }],
            )
            .await?;
        }
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok((task, moved))
    }
}
