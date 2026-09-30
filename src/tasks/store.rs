use sqlx::{FromRow, Sqlite, SqlitePool, Transaction, types::Json};
use uuid::Uuid;

use super::{
    Task, TaskAgentRun, TaskAgentTurnReport, TaskCurrentState, TaskError, TaskHistoryEvent,
    TaskOperatorAttentionLevel, TaskOperatorNotice,
};
use crate::{
    agent::AgentKind,
    db::Database,
    vcs::{CodeContribution, ContributionObservation},
};

// SQLx reads the JSON projection through Serde, sharing its enum spellings.
#[derive(FromRow)]
struct TaskRow {
    id: String,
    agent_kind: Option<AgentKind>,
    contributions: Json<Vec<CodeContribution>>,
    observations: Json<Vec<ContributionObservation>>,
    state: Json<TaskCurrentState>,
}

impl From<TaskRow> for Task {
    fn from(row: TaskRow) -> Self {
        Self {
            id: row.id,
            agent_kind: row.agent_kind,
            code_contributions: row.contributions.0,
            code_contribution_observations: row.observations.0,
            current_state: row.state.0,
        }
    }
}

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
        sqlx::query_as::<_, TaskRow>(
            r#"
            SELECT tasks.id, tasks.agent_kind, task_statuses.state,
                COALESCE((
                    SELECT json_group_array(json(contribution))
                    FROM task_code_contributions
                    WHERE task_id = tasks.id
                ), json('[]')) AS contributions,
                COALESCE((
                    SELECT json_group_array(json(observation))
                    FROM task_code_contribution_observations
                    WHERE task_id = tasks.id
                      AND sequence IN (
                          SELECT MAX(sequence)
                          FROM task_code_contribution_observations
                          WHERE task_id = tasks.id
                          GROUP BY json_extract(observation, '$.contribution_id')
                      )
                ), json('[]')) AS observations
            FROM tasks
            JOIN task_statuses ON task_statuses.task_id = tasks.id
            ORDER BY last_event_sequence DESC
        "#,
        )
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(Task::from).collect())
        .map_err(TaskError::Database)
    }

    pub async fn create_with_agent(
        &self,
        intent: String,
        agent_kind: Option<AgentKind>,
        agent_conversation_id: Option<String>,
    ) -> Result<Task, TaskError> {
        let task_id = Uuid::new_v4().to_string();
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        sqlx::query(
            r#"
            INSERT INTO tasks (id, agent_kind, created_at_ms)
            VALUES (?1, ?2, ?3)
        "#,
        )
        .bind(&task_id)
        .bind(agent_kind)
        .bind(now_ms())
        .execute(&mut *transaction)
        .await
        .map_err(TaskError::Database)?;
        Self::write_events(
            &mut transaction,
            &task_id,
            &[TaskHistoryEvent::IntentExpressed {
                intent,
                agent_conversation_id,
            }],
        )
        .await?;
        let task = Self::read_task(&mut transaction, &task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    /// Append a whole transition atomically and read its resulting state before releasing the writer.
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

    async fn read_task(
        transaction: &mut Transaction<'_, Sqlite>,
        task_id: &str,
    ) -> Result<Task, TaskError> {
        sqlx::query_as::<_, TaskRow>(
            r#"
            SELECT tasks.id, tasks.agent_kind, task_statuses.state,
                COALESCE((
                    SELECT json_group_array(json(contribution))
                    FROM task_code_contributions
                    WHERE task_id = tasks.id
                ), json('[]')) AS contributions,
                COALESCE((
                    SELECT json_group_array(json(observation))
                    FROM task_code_contribution_observations
                    WHERE task_id = tasks.id
                      AND sequence IN (
                          SELECT MAX(sequence)
                          FROM task_code_contribution_observations
                          WHERE task_id = tasks.id
                          GROUP BY json_extract(observation, '$.contribution_id')
                      )
                ), json('[]')) AS observations
            FROM tasks
            JOIN task_statuses ON task_statuses.task_id = tasks.id
            WHERE tasks.id = ?
        "#,
        )
        .bind(task_id)
        .fetch_one(&mut **transaction)
        .await
        .map(Task::from)
        .map_err(TaskError::Database)
    }

    async fn write_events(
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

    pub async fn record_report(
        &self,
        task_id: &str,
        agent_run: &TaskAgentRun,
        report: &TaskAgentTurnReport,
        contributions: &[CodeContribution],
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
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        Self::write_events(&mut transaction, task_id, &events).await?;
        for contribution in contributions {
            let event = TaskHistoryEvent::CodeContributionLinked {
                contribution: contribution.clone(),
            };
            let payload = serde_json::to_string(&event).map_err(TaskError::HistoryData)?;
            sqlx::query(
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
            .map_err(TaskError::Database)?;
        }
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok((task, notices))
    }

    /// Record observed contribution state for a task.
    ///
    /// The event id is derived from the observation itself, so a state already
    /// delivered collapses into the existing row instead of replaying the same
    /// feedback. That is what makes restarting Altitude safe.
    pub async fn record_code_contribution_observations(
        &self,
        task_id: &str,
        observations: &[ContributionObservation],
    ) -> Result<Task, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        for observation in observations {
            let event = TaskHistoryEvent::CodeContributionObserved {
                observation: observation.clone(),
            };
            let payload = serde_json::to_string(&event).map_err(TaskError::HistoryData)?;
            sqlx::query(
                r#"
                INSERT INTO task_event_logs (id, task_id, recorded_at_ms, payload)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(id) DO NOTHING
            "#,
            )
            .bind(format!(
                "code-contribution-observation:{task_id}:{}",
                observation.identity()
            ))
            .bind(task_id)
            .bind(now_ms())
            .bind(payload)
            .execute(&mut *transaction)
            .await
            .map_err(TaskError::Database)?;
        }
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    pub async fn unacknowledged_operator_notices(
        &self,
        level: TaskOperatorAttentionLevel,
    ) -> Result<Vec<TaskOperatorNotice>, TaskError> {
        sqlx::query_scalar::<_, Json<TaskOperatorNotice>>(
            r#"
            SELECT json_object(
                'id', id,
                'task_id', task_id,
                'level', level,
                'title', title,
                'body', body
            )
            FROM task_operator_notices
            WHERE level = json_extract(?1, '$') AND acknowledged = 0
            ORDER BY sequence DESC
        "#,
        )
        .bind(Json(level))
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(|row| row.0).collect())
        .map_err(TaskError::Database)
    }

    pub async fn acknowledge_operator_notice(&self, notice_id: &str) -> Result<(), TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let task_id: Option<String> = sqlx::query_scalar(
            r#"
            SELECT task_id
            FROM task_operator_notices
            WHERE id = ? AND acknowledged = 0
        "#,
        )
        .bind(notice_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(TaskError::Database)?;
        if let Some(task_id) = task_id {
            Self::write_events(
                &mut transaction,
                &task_id,
                &[TaskHistoryEvent::OperatorNoticeAcknowledged {
                    notice_id: notice_id.to_owned(),
                }],
            )
            .await?;
        }
        transaction.commit().await.map_err(TaskError::Database)?;
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
