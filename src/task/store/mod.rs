use std::cmp::Ordering;

use sqlx::{FromRow, Sqlite, SqlitePool, Transaction, types::Json};
use uuid::Uuid;

use crate::id;
use crate::project::Project;
use crate::task::{
    ProjectProposal, SignalTransition, Task, TaskAgentRun, TaskAgentTurnReport, TaskError,
    TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus, TaskFacts, TaskHistoryEvent,
    TaskInput, TaskInputAuthor, TaskInputSource, TaskOperatorAttentionLevel, TaskOperatorSignal,
    TaskOperatorSignalAuthor, TaskOperatorSignalDraft, TaskOperatorSignalKind,
    TaskOperatorSignalMessage, TaskOperatorSignalThread, TaskPlanStep,
};
use crate::{
    agent::AgentKind,
    db::Database,
    tracker::IssueReference,
    vcs::{CodeContribution, ContributionObservation},
};

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

    async fn write_recorded_events(
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
        review: ReviewRequest<'_>,
    ) -> Result<Task, TaskError> {
        let mut events = vec![TaskHistoryEvent::AgentTurnReported {
            agent_run_id: agent_run.id.clone(),
            requested_action: report.requested_action(),
            operator_attention_level: attention,
        }];
        if let Some(model) = report
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
        {
            events.push(TaskHistoryEvent::AgentModelObserved {
                model: model.to_owned(),
            });
        }
        // A run that is answering a thread keeps speaking on it. The thread id is
        // reused only while it is unresolved, and never for a review: a review is the
        // operator's to close, so the agent's next words open a thread of their own.
        let continuing = match &agent_run.answering_signal_id {
            Some(signal_id)
                if self.is_signal_unresolved(task_id, signal_id).await?
                    && self.thread_of(task_id, signal_id).await?.kind
                        != TaskOperatorSignalKind::InReview =>
            {
                Some(signal_id.clone())
            }
            _ => None,
        };
        let raised_id = match &continuing {
            Some(signal_id) => Some(signal_id.clone()),
            None if report.operator_signal.is_some() => Some(self.next_signal_id(task_id).await?),
            None => None,
        };
        let signal = |draft: &TaskOperatorSignalDraft| TaskOperatorSignal {
            id: raised_id.clone().unwrap_or_default(),
            task_id: task_id.to_owned(),
            kind: draft.kind,
            title: draft.title.clone(),
            body: draft.body.clone(),
            raised_at_ms: now_ms(),
            proposal: None,
        };
        match review {
            // A held request is not raised now; the host raises it later, once
            // the linked contributions can answer. Either way it is recorded
            // here, so the agent's request is never lost to a restart.
            ReviewRequest::Hold(draft) => events.push(TaskHistoryEvent::ReviewDeferred {
                signal: signal(draft),
                agent_run_id: agent_run.id.clone(),
            }),
            ReviewRequest::Raise => events.extend(report.operator_signal.iter().map(|draft| {
                TaskHistoryEvent::OperatorSignalRaised {
                    signal: signal(draft),
                }
            })),
        }
        // A report that asks for nothing resolves the thread it was continuing:
        // the agent has what it needed and moved on.
        if report.operator_signal.is_none()
            && let Some(signal_id) = &continuing
        {
            events.push(TaskHistoryEvent::OperatorSignalResolved {
                signal_id: signal_id.clone(),
                resolved: true,
                by: TaskOperatorSignalAuthor::Agent,
            });
        }
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(TaskError::Database)?;
        Self::write_events(&mut transaction, task_id, &events).await?;
        let task = Self::read_task(&mut transaction, task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
    }

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

    pub async fn inputs(&self, task_id: &str) -> Result<Vec<TaskInput>, TaskError> {
        let mut inputs = Vec::new();
        for (at_ms, event) in self.events(task_id).await? {
            match event {
                TaskHistoryEvent::IntentExpressed {
                    intent,
                    identity_id,
                    identity_label,
                    ..
                } if inputs.is_empty() => {
                    inputs.push(TaskInput {
                        id: format!("founding:{task_id}"),
                        at_ms,
                        author: TaskInputAuthor::Operator {
                            id: identity_id,
                            label: identity_label,
                        },
                        source: TaskInputSource::TaskCreated,
                        body: intent,
                        responding_to: None,
                        external_id: None,
                    });
                }
                TaskHistoryEvent::TaskInputExpressed { input } => inputs.push(input),
                TaskHistoryEvent::OperatorSignalMessage {
                    signal_id,
                    author: TaskOperatorSignalAuthor::Operator,
                    body,
                } => {
                    let task = self.get(task_id).await?;
                    let Some(task) = task else { continue };
                    inputs.push(TaskInput {
                        id: format!("legacy-signal:{task_id}:{signal_id}:{at_ms}"),
                        at_ms,
                        author: TaskInputAuthor::Operator {
                            id: task.facts.operator_id.unwrap_or_default(),
                            label: task.facts.operator_label.unwrap_or_default(),
                        },
                        source: TaskInputSource::Altitude,
                        body,
                        responding_to: Some(signal_id),
                        external_id: None,
                    });
                }
                _ => {}
            }
        }
        Ok(inputs)
    }

    pub async fn has_unanswered_question(&self, task_id: &str) -> Result<bool, TaskError> {
        Ok(self
            .unresolved_signal_threads(task_id)
            .await?
            .iter()
            .any(|thread| {
                matches!(
                    thread.kind,
                    TaskOperatorSignalKind::Blocked | TaskOperatorSignalKind::InReview
                ) && !thread
                    .messages
                    .iter()
                    .any(|message| message.author == TaskOperatorSignalAuthor::Operator)
            }))
    }

    pub async fn unresolved_signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignalThread>, TaskError> {
        Ok(self
            .signal_threads(task_id)
            .await?
            .into_iter()
            .filter(TaskOperatorSignalThread::is_unresolved)
            .collect())
    }

    pub async fn signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignalThread>, TaskError> {
        let mut threads: Vec<TaskOperatorSignalThread> = Vec::new();
        for (at_ms, event) in self.events(task_id).await? {
            match event {
                TaskHistoryEvent::OperatorSignalRaised { signal } => {
                    match threads.iter_mut().find(|thread| thread.id == signal.id) {
                        Some(thread) => {
                            thread.kind = signal.kind;
                            thread.title = signal.title.clone();
                            thread.last_raised_at_ms = at_ms;
                            thread.proposal = signal.proposal.clone();
                            thread.resolved = false;
                            thread.resolved_by = None;
                            thread.resolved_at_ms = None;
                            thread.messages.push(TaskOperatorSignalMessage {
                                id: format!("m{}", thread.messages.len() + 1),
                                author: TaskOperatorSignalAuthor::Agent,
                                body: signal.body,
                                at_ms,
                            });
                        }
                        None => threads.push(TaskOperatorSignalThread {
                            id: signal.id,
                            task_id: signal.task_id,
                            kind: signal.kind,
                            title: signal.title,
                            opened_at_ms: signal.raised_at_ms,
                            last_raised_at_ms: at_ms,
                            messages: vec![TaskOperatorSignalMessage {
                                id: "m1".to_owned(),
                                author: TaskOperatorSignalAuthor::Agent,
                                body: signal.body,
                                at_ms,
                            }],
                            resolved: false,
                            resolved_by: None,
                            resolved_at_ms: None,
                            transitions: Vec::new(),
                            proposal: signal.proposal,
                        }),
                    }
                }
                TaskHistoryEvent::TaskInputExpressed { input } => {
                    let Some(signal_id) = input.responding_to.as_deref() else {
                        continue;
                    };
                    if let Some(thread) = threads.iter_mut().find(|thread| thread.id == signal_id) {
                        thread.resolved = false;
                        thread.resolved_by = None;
                        thread.resolved_at_ms = None;
                        thread.messages.push(TaskOperatorSignalMessage {
                            id: format!("m{}", thread.messages.len() + 1),
                            author: match input.author {
                                TaskInputAuthor::Operator { .. } => {
                                    TaskOperatorSignalAuthor::Operator
                                }
                                TaskInputAuthor::Participant { .. } => {
                                    TaskOperatorSignalAuthor::Operator
                                }
                            },
                            body: input.body,
                            at_ms,
                        });
                    }
                }
                TaskHistoryEvent::OperatorSignalMessage {
                    signal_id,
                    author,
                    body,
                } => {
                    if let Some(thread) = threads.iter_mut().find(|thread| thread.id == signal_id) {
                        // A message is the whole conversation. It reopens a
                        // resolved thread; the other side sees it when it reads.
                        thread.resolved = false;
                        thread.resolved_by = None;
                        thread.resolved_at_ms = None;
                        thread.messages.push(TaskOperatorSignalMessage {
                            id: format!("m{}", thread.messages.len() + 1),
                            author,
                            body,
                            at_ms,
                        });
                    }
                }
                TaskHistoryEvent::OperatorSignalResolved {
                    signal_id,
                    resolved,
                    by,
                } => {
                    if let Some(thread) = threads.iter_mut().find(|thread| thread.id == signal_id) {
                        thread.resolved = resolved;
                        thread.resolved_by = resolved.then_some(by);
                        thread.resolved_at_ms = resolved.then_some(at_ms);
                        thread.transitions.push(SignalTransition {
                            at_ms,
                            resolved,
                            by,
                        });
                    }
                }
                TaskHistoryEvent::OperatorSignalConfirmed { signal_id } => {
                    if let Some(thread) = threads.iter_mut().find(|thread| thread.id == signal_id) {
                        thread.resolved = true;
                        thread.resolved_by = Some(TaskOperatorSignalAuthor::Operator);
                        thread.resolved_at_ms = Some(at_ms);
                    }
                }
                _ => {}
            }
        }
        // Unresolved first, in the order the operator has to deal with them:
        // blocked and review share the top tier, decisions are context, and
        // updates are the log. Within a tier the oldest is highest, so a thread
        // does not jump when somebody replies.
        threads.sort_by(|left, right| match (left.resolved, right.resolved) {
            (false, false) => left
                .kind
                .tier()
                .cmp(&right.kind.tier())
                .then(left.opened_at_ms.cmp(&right.opened_at_ms)),
            (false, true) => Ordering::Less,
            (true, false) => Ordering::Greater,
            (true, true) => right
                .resolved_at_ms
                .cmp(&left.resolved_at_ms)
                .then(right.opened_at_ms.cmp(&left.opened_at_ms)),
        });
        Ok(threads)
    }

    pub async fn signal_thread(
        &self,
        task_id: &str,
        signal_id: &str,
    ) -> Result<Option<TaskOperatorSignalThread>, TaskError> {
        Ok(self
            .signal_threads(task_id)
            .await?
            .into_iter()
            .find(|thread| thread.id == signal_id))
    }

    pub async fn message_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        body: &str,
    ) -> Result<Task, TaskError> {
        self.message_signal_as(task_id, signal_id, TaskOperatorSignalAuthor::Agent, body)
            .await
    }

    pub async fn record_plan(
        &self,
        task_id: &str,
        steps: Vec<TaskPlanStep>,
    ) -> Result<Task, TaskError> {
        self.append_events(task_id, &[TaskHistoryEvent::PlanUpdated { steps }])
            .await
    }

    pub async fn message_operator_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        body: &str,
    ) -> Result<Task, TaskError> {
        self.thread_of(task_id, signal_id).await?;
        let task = self
            .get(task_id)
            .await?
            .ok_or(TaskError::OperationRejected(
                "this task is no longer in play",
            ))?;
        let input = TaskInput {
            id: Uuid::new_v4().to_string(),
            at_ms: now_ms(),
            author: TaskInputAuthor::Operator {
                id: task.facts.operator_id.unwrap_or_default(),
                label: task.facts.operator_label.unwrap_or_default(),
            },
            source: TaskInputSource::Altitude,
            body: body.to_owned(),
            responding_to: Some(signal_id.to_owned()),
            external_id: None,
        };
        self.append_events(task_id, &[TaskHistoryEvent::TaskInputExpressed { input }])
            .await
    }

    async fn message_signal_as(
        &self,
        task_id: &str,
        signal_id: &str,
        author: TaskOperatorSignalAuthor,
        body: &str,
    ) -> Result<Task, TaskError> {
        self.thread_of(task_id, signal_id).await?;
        self.append_events(
            task_id,
            &[TaskHistoryEvent::OperatorSignalMessage {
                signal_id: signal_id.to_owned(),
                author,
                body: body.to_owned(),
            }],
        )
        .await
    }

    pub async fn resolve_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        resolved: bool,
        by: TaskOperatorSignalAuthor,
    ) -> Result<Task, TaskError> {
        self.thread_of(task_id, signal_id).await?;
        self.append_events(
            task_id,
            &[TaskHistoryEvent::OperatorSignalResolved {
                signal_id: signal_id.to_owned(),
                resolved,
                by,
            }],
        )
        .await
    }

    pub(crate) async fn next_signal_id(&self, task_id: &str) -> Result<String, TaskError> {
        loop {
            let candidate = id::signal();
            let exists: Option<i64> = sqlx::query_scalar(
                r#"
                SELECT 1 FROM task_operator_signals WHERE task_id = ?1 AND id = ?2
                UNION ALL
                SELECT 1 FROM task_deferred_reviews
                WHERE task_id = ?1 AND json_extract(payload, '$.data.signal.id') = ?2
                "#,
            )
            .bind(task_id)
            .bind(&candidate)
            .fetch_optional(&self.pool)
            .await
            .map_err(TaskError::Database)?;
            if exists.is_none() {
                return Ok(candidate);
            }
        }
    }

    pub async fn open_signal(
        &self,
        task_id: &str,
        kind: TaskOperatorSignalKind,
        title: &str,
        body: &str,
        proposal: Option<ProjectProposal>,
    ) -> Result<(Task, String), TaskError> {
        // A blocked thread raised mid-turn is a request the operator can act on
        // here, rather than a question only the report can ask: the proposal is
        // what they would apply.
        let actionable = kind == TaskOperatorSignalKind::Blocked
            && proposal
                .as_ref()
                .is_some_and(|proposal| !proposal.is_empty());
        if !matches!(
            kind,
            TaskOperatorSignalKind::Update | TaskOperatorSignalKind::Decision
        ) && !actionable
        {
            return Err(TaskError::OperationRejected(
                "only updates and decisions can be opened mid-turn",
            ));
        }
        let task = self
            .get(task_id)
            .await?
            .ok_or(TaskError::OperationRejected(
                "this task is no longer in play",
            ))?;
        if task.facts.accepted {
            return Err(TaskError::OperationRejected(
                "this task has already been accepted",
            ));
        }
        let signal = TaskOperatorSignal {
            id: self.next_signal_id(task_id).await?,
            task_id: task_id.to_owned(),
            kind,
            title: title.to_owned(),
            body: body.to_owned(),
            raised_at_ms: now_ms(),
            proposal,
        };
        let id = signal.id.clone();
        let task = self
            .append_events(
                task_id,
                &[TaskHistoryEvent::OperatorSignalRaised { signal }],
            )
            .await?;
        Ok((task, id))
    }

    async fn thread_of(
        &self,
        task_id: &str,
        signal_id: &str,
    ) -> Result<TaskOperatorSignalThread, TaskError> {
        self.signal_thread(task_id, signal_id)
            .await?
            .ok_or(TaskError::OperationRejected(
                "no such signal thread on this task",
            ))
    }

    pub async fn is_signal_unresolved(
        &self,
        task_id: &str,
        signal_id: &str,
    ) -> Result<bool, TaskError> {
        let unresolved: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM task_operator_signals WHERE task_id = ?1 AND id = ?2 AND resolved = 0",
        )
        .bind(task_id)
        .bind(signal_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(TaskError::Database)?;
        Ok(unresolved.is_some())
    }

    pub async fn confirm_operator_signal(
        &self,
        task_id: &str,
        signal_id: &str,
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
            WHERE task_id = ?1 AND id = ?2 AND resolved = 0
              AND EXISTS (SELECT 1 FROM tasks WHERE tasks.id = task_operator_signals.task_id AND removed_at_ms IS NULL)
              AND NOT EXISTS (SELECT 1 FROM task_event_logs WHERE task_id = task_operator_signals.task_id AND event_type IN ('operator_signal_confirmed', 'task_completed'))
        "#,
        )
        .bind(task_id)
        .bind(signal_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(TaskError::Database)?;
        if kind.0 != TaskOperatorSignalKind::InReview {
            return Err(TaskError::OperationRejected(
                "only a review request can be accepted",
            ));
        }
        let task = Self::read_task(&mut transaction, &task_id).await?;
        if task.facts.execution_status != TaskExecutionStatus::Paused {
            return Err(TaskError::OperationRejected(
                "wait until the task is stopped before accepting it",
            ));
        }
        Self::write_events(
            &mut transaction,
            &task_id,
            &[TaskHistoryEvent::OperatorSignalConfirmed {
                signal_id: signal_id.to_owned(),
            }],
        )
        .await?;
        let task = Self::read_task(&mut transaction, &task_id).await?;
        transaction.commit().await.map_err(TaskError::Database)?;
        Ok(task)
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
