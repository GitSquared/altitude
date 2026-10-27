//! What a task tells its operator, and what the operator answers.
//!
//! Signals are conversations. A reply is a message, not a typed command: the
//! agent reads it and decides what it means. Resolving a thread is the one
//! explicit state. Only accepting a review request also ends the task.

use crate::isolation::{CommandSession, IsolationEngine};

use super::{
    Task, TaskAgentRun, TaskError, TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent,
    TaskInput, TaskInputAuthor, TaskInputSource, TaskOperatorSignalAuthor,
    TaskOperatorSignalThread, TaskService, TaskServiceError,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    /// The task that owns a signal, when this machine knows the thread.
    pub async fn task_for_signal(
        &self,
        task_id: &str,
        signal_id: &str,
    ) -> Result<Option<Task>, TaskServiceError> {
        self.store
            .task_for_signal(task_id, signal_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    /// The inputs that express what the operator has asked for, oldest first.
    pub async fn inputs(&self, task_id: &str) -> Result<Vec<TaskInput>, TaskServiceError> {
        self.store
            .inputs(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    /// Add a new message from the operator to the expressed intent.
    ///
    /// The message is recorded before any turn starts. If the task is already
    /// running, it is queued for the next turn rather than interrupting the
    /// current one.
    pub async fn express_input(
        &self,
        task_id: &str,
        body: String,
    ) -> Result<Option<(Task, TaskAgentRun, CommandSession)>, TaskServiceError> {
        let task = self
            .store
            .get(task_id)
            .await
            .map_err(TaskServiceError::Task)?
            .ok_or(TaskServiceError::Task(TaskError::OperationRejected(
                "this task is no longer in play",
            )))?;
        if task.facts.accepted {
            return Err(TaskServiceError::Task(TaskError::OperationRejected(
                "this task has already been accepted",
            )));
        }
        if task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer) {
            return Err(TaskServiceError::Task(TaskError::OperationRejected(
                "the task is awaiting transfer confirmation",
            )));
        }
        let input = TaskInput {
            id: uuid::Uuid::new_v4().to_string(),
            at_ms: chrono::Utc::now().timestamp_millis(),
            author: TaskInputAuthor::Operator {
                id: task.facts.operator_id.clone().unwrap_or_default(),
                label: task.facts.operator_label.clone().unwrap_or_default(),
            },
            source: TaskInputSource::Altitude,
            body,
            responding_to: None,
            external_id: None,
        };
        let mut task = self
            .record(&task.id, &[TaskHistoryEvent::TaskInputExpressed { input }])
            .await?;
        if matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Running | TaskExecutionStatus::Pending
        ) {
            return Ok(None);
        }
        if !task.facts.sandbox_available {
            task = self.provision(task).await?;
        }
        let intends = self.inputs(&task.id).await?;
        let prompt = format!(
            "The operator sent a new message. Read it and the task, answer through a signal              if needed, and continue autonomously.\n\nExpressed intent so far:\n{}",
            render_inputs(&intends)
        );
        let (task, run, session) = self.start_autopilot_turn(task, &prompt, None).await?;
        Ok(Some((task, run, session)))
    }

    /// The threads that still need an answer or an ending.
    pub async fn unresolved_signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignalThread>, TaskServiceError> {
        self.store
            .unresolved_signal_threads(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    /// Every thread a task has, resolved ones included.
    pub async fn signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignalThread>, TaskServiceError> {
        self.store
            .signal_threads(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    /// Resolve or reopen a thread. Resolving does not wake the agent and does not
    /// change the task; accepting a review is the separate task-ending action.
    pub async fn resolve_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        resolved: bool,
    ) -> Result<Task, TaskServiceError> {
        self.store
            .resolve_signal(
                task_id,
                signal_id,
                resolved,
                TaskOperatorSignalAuthor::Operator,
            )
            .await
            .map_err(TaskServiceError::Task)
    }

    /// Reply in a thread. The message is recorded first; if the task is not
    /// already running, it is woken to read it. What the reply means is the
    /// agent's to decide.
    pub async fn reply_operator_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        reply: String,
    ) -> Result<Option<(Task, TaskAgentRun, CommandSession)>, TaskServiceError> {
        // Re-read rather than trusting a caller-held task: the operator may have
        // opened the signal list some time before replying.
        let mut task = self
            .store
            .message_operator_signal(task_id, signal_id, &reply)
            .await
            .map_err(TaskServiceError::Task)?;
        if task.facts.accepted
            || matches!(
                task.facts.execution_status,
                TaskExecutionStatus::Running | TaskExecutionStatus::Pending
            )
            || task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer)
        {
            return Ok(None);
        }
        if !task.facts.sandbox_available {
            task = self.provision(task).await?;
        }
        // The turn reads the whole task and the thread; it does not receive a
        // pre-digested command. Keeping the answering id lets a report that has
        // nothing to add resolve the thread it was continuing.
        let (task, run, session) = self
            .start_autopilot_turn(
                task,
                &format!(
                    "The operator replied on signal {signal_id}. Read the thread and the task, answer in the thread, and do the work the reply asks for. Continue autonomously if there is more to do."
                ),
                Some(signal_id.to_owned()),
            )
            .await?;
        Ok(Some((task, run, session)))
    }

    /// Accept a wrap-up request: the operator's judgment, and the only ending.
    ///
    /// The judgment is recorded before the capsule is touched, so what the
    /// operator decided does not depend on cleanup succeeding. Nothing here ends a
    /// run: the run that asked for review already ended, waiting for this answer.
    pub async fn confirm_operator_signal(
        &self,
        task_id: &str,
        signal_id: &str,
    ) -> Result<Task, TaskServiceError> {
        let task = match self.store.confirm_operator_signal(task_id, signal_id).await {
            Ok(task) => task,
            Err(error) => {
                let existing = self
                    .store
                    .task_for_signal(task_id, signal_id)
                    .await
                    .map_err(TaskServiceError::Task)?;
                let Some(task) = existing else {
                    return Err(TaskServiceError::Task(error));
                };
                let events = self
                    .store
                    .events(&task.id)
                    .await
                    .map_err(TaskServiceError::Task)?;
                let already_confirmed = events.iter().any(|(_, event)| {
                    matches!(
                        event,
                        TaskHistoryEvent::OperatorSignalConfirmed { signal_id: confirmed }
                            if confirmed == signal_id
                    )
                });
                if !already_confirmed {
                    return Err(TaskServiceError::Task(error));
                }
                task
            }
        };
        let task = self.release_resources(&task).await?;
        self.record(
            &task.id,
            &[TaskHistoryEvent::TaskCompleted {
                execution_status: TaskExecutionStatus::Completed,
            }],
        )
        .await
    }
}

/// The input log as prose for a turn that needs the whole expressed intent.
fn render_inputs(inputs: &[TaskInput]) -> String {
    let mut text = String::new();
    for input in inputs {
        let source = match &input.source {
            TaskInputSource::TaskCreated => "task created".to_owned(),
            TaskInputSource::Altitude => "Altitude".to_owned(),
            TaskInputSource::TrackerComment { provider, .. } => {
                format!("{} comment", provider.label())
            }
        };
        let reply = input
            .responding_to
            .as_deref()
            .map(|signal| format!(", reply to signal {signal}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "- [{} · {source}{reply}] {}\n",
            chrono::DateTime::from_timestamp_millis(input.at_ms)
                .map(|at| at
                    .with_timezone(&chrono::Local)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .unwrap_or_default(),
            input.body.trim()
        ));
    }
    text
}
