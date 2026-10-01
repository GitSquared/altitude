use sqlx::{FromRow, Sqlite, SqlitePool, Transaction, types::Json};
use std::io;
use uuid::Uuid;

use super::{
    Task, TaskAgentRun, TaskAgentTurnReport, TaskCurrentState, TaskError, TaskExecutionPauseReason,
    TaskExecutionStatus, TaskHistoryEvent, TaskOperatorAttentionLevel, TaskOperatorSignal,
    TaskOperatorSignalKind,
};
use crate::development::DevelopmentEnvironment;
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
        environment: DevelopmentEnvironment,
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
                environment,
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
        attention: TaskOperatorAttentionLevel,
        contributions: &[CodeContribution],
    ) -> Result<Task, TaskError> {
        let mut events = vec![TaskHistoryEvent::AgentTurnReported {
            agent_run_id: agent_run.id.clone(),
            requested_action: report.requested_action.clone(),
            operator_attention_level: attention,
            summary: report.summary.clone(),
        }];
        let signals: Vec<_> = report
            .operator_signal
            .iter()
            .map(|draft| TaskOperatorSignal {
                id: Uuid::new_v4().to_string(),
                task_id: task_id.to_owned(),
                kind: draft.kind,
                priority: draft.priority,
                title: draft.title.clone(),
                body: draft.body.clone(),
                raised_at_ms: now_ms(),
                read: false,
            })
            .collect();
        events.extend(
            signals
                .iter()
                .cloned()
                .map(|signal| TaskHistoryEvent::OperatorSignalRaised { signal }),
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
        Ok(task)
    }

    /// Record transitions, including a return to a previously observed state.
    /// Compare with the latest event under the write lock so unchanged polls
    /// remain a no-op across restarts without suppressing A -> B -> A.
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
        Ok(task)
    }

    pub async fn operator_signals(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignal>, TaskError> {
        sqlx::query_scalar::<_, Json<TaskOperatorSignal>>(
            r#"
            SELECT json_object(
                'id', id,
                'task_id', task_id,
                'kind', kind,
                'priority', priority,
                'title', title,
                'body', body,
                'raised_at_ms', raised_at_ms,
                'read', json(CASE WHEN read THEN 'true' ELSE 'false' END)
            )
            FROM task_operator_signals
            WHERE task_id = ?1 AND resolved = 0
            ORDER BY
                CASE priority
                    WHEN 'urgent' THEN 0
                    WHEN 'high' THEN 1
                    WHEN 'normal' THEN 2
                    WHEN 'low' THEN 3
                END,
                CASE kind
                    WHEN 'decision' THEN 0
                    WHEN 'review' THEN 1
                    WHEN 'update' THEN 2
                END,
                raised_at_ms ASC
        "#,
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(|row| row.0).collect())
        .map_err(TaskError::Database)
    }

    pub async fn read_operator_signals(&self, task_id: &str) -> Result<Task, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let signal_ids: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT id
            FROM task_operator_signals
            WHERE task_id = ?1 AND read = 0 AND resolved = 0
        "#,
        )
        .bind(task_id)
        .fetch_all(&mut *transaction)
        .await
        .map_err(TaskError::Database)?;
        let events: Vec<_> = signal_ids
            .into_iter()
            .map(|signal_id| TaskHistoryEvent::OperatorSignalRead { signal_id })
            .collect();
        Self::write_events(&mut transaction, task_id, &events).await?;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    pub async fn acknowledge_operator_signal(&self, signal_id: &str) -> Result<Task, TaskError> {
        self.resolve_operator_signal(signal_id, None).await
    }

    pub async fn answer_operator_signal(
        &self,
        signal_id: &str,
        answer: &str,
    ) -> Result<Task, TaskError> {
        self.resolve_operator_signal(signal_id, Some(answer)).await
    }

    async fn resolve_operator_signal(
        &self,
        signal_id: &str,
        answer: Option<&str>,
    ) -> Result<Task, TaskError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        let (task_id, kind): (String, Json<TaskOperatorSignalKind>) = sqlx::query_as(
            r#"
            SELECT task_id, json_quote(kind)
            FROM task_operator_signals
            WHERE id = ?1 AND resolved = 0
        "#,
        )
        .bind(signal_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(TaskError::Database)?;
        if answer.is_some() {
            let task = Self::read_task(&mut transaction, &task_id).await?;
            if task.current_state.execution_status != TaskExecutionStatus::Paused
                || task.current_state.execution_pause_reason
                    != Some(TaskExecutionPauseReason::AwaitingOperator)
            {
                return Err(TaskError::HistoryData(serde_json::Error::io(
                    io::Error::other(
                        "wait until the task is paused for the operator before answering",
                    ),
                )));
            }
        }
        let event = match (kind.0, answer) {
            (TaskOperatorSignalKind::Update, None) => {
                TaskHistoryEvent::OperatorSignalAcknowledged {
                    signal_id: signal_id.to_owned(),
                }
            }
            (TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::Review, Some(answer)) => {
                TaskHistoryEvent::OperatorSignalAnswered {
                    signal_id: signal_id.to_owned(),
                    answer: answer.to_owned(),
                }
            }
            _ => {
                return Err(TaskError::HistoryData(serde_json::Error::io(
                    io::Error::other(
                        "updates are acknowledged; decisions and reviews are answered",
                    ),
                )));
            }
        };
        Self::write_events(&mut transaction, &task_id, &[event]).await?;
        let task = Self::read_task(&mut transaction, &task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
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
