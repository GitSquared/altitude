use sqlx::{FromRow, Sqlite, SqlitePool, Transaction, types::Json};
use uuid::Uuid;

use super::{
    Task, TaskAgentRun, TaskAgentTurnReport, TaskError, TaskExecutionMode,
    TaskExecutionPauseReason, TaskExecutionStatus, TaskFacts, TaskHistoryEvent,
    TaskOperatorAttentionLevel, TaskOperatorSignal, TaskOperatorSignalKind,
};
use crate::environment::Environment;
use crate::{
    agent::AgentKind,
    db::Database,
    vcs::{CodeContribution, ContributionObservation},
};

// SQL returns the latest recorded properties without interpreting task lifecycle.
#[derive(FromRow)]
struct TaskRow {
    id: String,
    agent_kind: Option<AgentKind>,
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
            code_contributions: row.contributions.0,
            code_contribution_observations: row.observations.0,
            facts: row.facts,
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
        sqlx::query_as::<_, TaskRow>(concat!(
            include_str!("read_task.sql"),
            " ORDER BY last_event_sequence DESC"
        ))
        .fetch_all(&self.pool)
        .await
        .map(|rows| rows.into_iter().map(Task::from).collect())
        .map_err(TaskError::Database)
    }

    pub async fn create_with_agent(
        &self,
        intent: String,
        environment: Environment,
        identity_id: String,
        identity_label: String,
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
                execution_status: TaskExecutionStatus::Pending,
                execution_mode: TaskExecutionMode::Autopilot,
                operator_attention_level: TaskOperatorAttentionLevel::Nominal,
                sandbox_available: false,
                snapshot_available: false,
                intent,
                environment,
                identity_id,
                identity_label,
                agent_conversation_id,
            }],
        )
        .await?;
        let task = Self::read_task(&mut transaction, &task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

    /// Append related facts atomically and read their latest values before releasing the writer.
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

    /// Record that the operator asked for a different driver.
    ///
    /// The run is named because only it can hand over; the record is what makes
    /// the request visible in a task's history afterwards, whatever happened to it.
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
                    WHEN 'failure' THEN -1
                    WHEN 'decision' THEN 0
                    WHEN 'in_review' THEN 1
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
        self.resolve_operator_signal(signal_id, SignalResolution::Acknowledge)
            .await
    }

    pub async fn answer_operator_signal(
        &self,
        signal_id: &str,
        answer: &str,
    ) -> Result<Task, TaskError> {
        self.resolve_operator_signal(signal_id, SignalResolution::Answer(answer))
            .await
    }

    /// Record that the operator accepted a wrap-up request.
    ///
    /// This resolves the signal but does not end the task: closing it out is the
    /// caller's business, because that is where the capsule is disposed of.
    pub async fn confirm_operator_signal(&self, signal_id: &str) -> Result<Task, TaskError> {
        self.resolve_operator_signal(signal_id, SignalResolution::Confirm)
            .await
    }

    async fn resolve_operator_signal(
        &self,
        signal_id: &str,
        resolution: SignalResolution<'_>,
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
        // Acknowledging an update asks nothing of the task; confirming or replying
        // needs it stopped, so the operator is answering something that is waiting.
        if resolution.is_answer() {
            let task = Self::read_task(&mut transaction, &task_id).await?;
            let failed = kind.0 == TaskOperatorSignalKind::Failure
                && task.facts.execution_status == TaskExecutionStatus::Failed;
            let awaiting_operator = task.facts.execution_status == TaskExecutionStatus::Paused
                && task.facts.execution_pause_reason
                    == Some(TaskExecutionPauseReason::AwaitingOperator);
            if !failed && !awaiting_operator {
                return Err(TaskError::OperationRejected(
                    "wait until the task is stopped for the operator before answering",
                ));
            }
        }
        let event = match (kind.0, &resolution) {
            (TaskOperatorSignalKind::Update, SignalResolution::Acknowledge) => {
                TaskHistoryEvent::OperatorSignalAcknowledged {
                    signal_id: signal_id.to_owned(),
                }
            }
            (TaskOperatorSignalKind::InReview, SignalResolution::Confirm) => {
                TaskHistoryEvent::OperatorSignalConfirmed {
                    signal_id: signal_id.to_owned(),
                }
            }
            (
                TaskOperatorSignalKind::Decision
                | TaskOperatorSignalKind::InReview
                | TaskOperatorSignalKind::Failure,
                SignalResolution::Answer(answer),
            ) => TaskHistoryEvent::OperatorSignalAnswered {
                signal_id: signal_id.to_owned(),
                answer: (*answer).to_owned(),
            },
            _ => {
                return Err(TaskError::OperationRejected(
                    "updates are acknowledged; wrap-up requests are confirmed; decisions, reviews, and failures are answered",
                ));
            }
        };
        let mut events = vec![event];
        if resolution.is_answer() {
            // Queue continuation durably with the answer so restart can pick it up.
            events.push(TaskHistoryEvent::ExecutionQueued {
                execution_status: TaskExecutionStatus::Pending,
            });
        }
        Self::write_events(&mut transaction, &task_id, &events).await?;
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

/// What the operator did with a signal. One verb per kind, because the kinds mean
/// different things: an update is dismissed, a wrap-up request is accepted, and a
/// question is replied to.
enum SignalResolution<'a> {
    Acknowledge,
    Confirm,
    Answer(&'a str),
}

impl SignalResolution<'_> {
    /// Whether the operator is answering something that was waiting for them.
    fn is_answer(&self) -> bool {
        matches!(self, Self::Confirm | Self::Answer(_))
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
