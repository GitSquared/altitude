use super::*;

use std::cmp::Ordering;

use sqlx::types::Json;
use uuid::Uuid;

use crate::id;
use crate::task::{
    ProjectProposal, SignalTransition, Task, TaskError, TaskExecutionStatus, TaskHistoryEvent,
    TaskInput, TaskInputAuthor, TaskInputSource, TaskOperatorSignal, TaskOperatorSignalAuthor,
    TaskOperatorSignalKind, TaskOperatorSignalMessage, TaskOperatorSignalThread, TaskPlanStep,
};

impl TaskStore {
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

    pub(super) async fn message_signal_as(
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
        // A review is one question with one answer — this milestone, to you — so a second
        // one while the first is still open asks it twice. Whether the operator has
        // answered the first does not change that: an answered review is still the review
        // they are looking at, and what has changed belongs in it.
        if kind == TaskOperatorSignalKind::InReview
            && self
                .unresolved_signal_threads(task_id)
                .await?
                .iter()
                .any(|thread| thread.kind == TaskOperatorSignalKind::InReview)
        {
            return Err(TaskError::OperationRejected(
                "a review of this task is already open; reply in it with what changed instead \
                 of asking for another",
            ));
        }
        // One request is one thread. A second question while the first is open is
        // the machine being asked to hold two waits at once, and it costs the
        // operator a second thread to read instead of an answer to the first.
        if matches!(
            kind,
            TaskOperatorSignalKind::Blocked | TaskOperatorSignalKind::InReview
        ) {
            // Two blockers wear one coat here and they are not the same wait: a
            // thread the operator has not answered is theirs to answer, and a review
            // this machine is holding for its pull requests is nobody's — it becomes
            // a thread by itself once they settle. Saying which one it is is the
            // difference between an instruction and a puzzle.
            if task.facts.deferred_review.is_some() {
                return Err(TaskError::OperationRejected(
                    "this task's review is already asked for and held until its pull requests \
                     settle; it becomes a thread by itself then, and nothing about it waits on \
                     the operator",
                ));
            }
            if self.has_unanswered_question(task_id).await? {
                return Err(TaskError::OperationRejected(
                    "this task is already waiting on the operator in a thread they have not \
                     answered; open a new one only once that has been answered",
                ));
            }
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
        // A review with pull requests to wait for is held rather than raised: the
        // host watches them and delivers it when they can answer, so the operator
        // is asked to look at work that is actually reviewable. One with nothing to
        // watch goes out at once.
        let event =
            if kind == TaskOperatorSignalKind::InReview && !task.code_contributions.is_empty() {
                TaskHistoryEvent::ReviewDeferred { signal }
            } else {
                TaskHistoryEvent::OperatorSignalRaised { signal }
            };
        let task = self.append_events(task_id, &[event]).await?;
        Ok((task, id))
    }

    pub(super) async fn thread_of(
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
}
