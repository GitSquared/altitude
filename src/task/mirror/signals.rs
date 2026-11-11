use crate::isolation::IsolationEngine;

use crate::task::{
    Task, TaskAsk, TaskAskAuthor, TaskError, TaskExecutionPauseReason, TaskExecutionStatus,
    TaskHistoryEvent, TaskInput, TaskInputAuthor, TaskInputSource, TaskService, TaskServiceError,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone + 'static,
{
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

    pub async fn inputs(&self, task_id: &str) -> Result<Vec<TaskInput>, TaskServiceError> {
        self.store
            .inputs(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn express_input(&self, task_id: &str, body: String) -> Result<(), TaskServiceError> {
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
            external_id: None,
        };
        let awake = matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Running | TaskExecutionStatus::Pending
        );
        let taken = if awake {
            TaskHistoryEvent::Steer
        } else {
            TaskHistoryEvent::Wake
        };
        let task = self
            .record(
                &task.id,
                &[TaskHistoryEvent::TaskInputExpressed { input }, taken],
            )
            .await?;
        if awake {
            return Ok(());
        }
        let intends = self.inputs(&task.id).await?;
        let prompt = format!(
            "The operator sent a new message. Read it and the task, answer through a signal              if needed, and continue autonomously.\n\nExpressed intent so far:\n{}",
            render_inputs(&intends)
        );
        self.queue_wake(&task, prompt).await
    }

    // A reaction is not an instruction: it is read by a run that is already going, and a
    // task that is not working has not asked for a sandbox to be built so it can hear
    // about an emoji. Nothing is said in the thread either, because an operator's message
    // there is their answer and this is not one.
    pub async fn note_operator_reaction(
        &self,
        task_id: &str,
        note: String,
    ) -> Result<(), TaskServiceError> {
        let task = self
            .store
            .get(task_id)
            .await
            .map_err(TaskServiceError::Task)?
            .ok_or(TaskServiceError::Task(TaskError::OperationRejected(
                "this task is no longer in play",
            )))?;
        if !matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Running | TaskExecutionStatus::Pending
        ) {
            return Ok(());
        }
        let input = TaskInput {
            id: uuid::Uuid::new_v4().to_string(),
            at_ms: chrono::Utc::now().timestamp_millis(),
            author: TaskInputAuthor::Operator {
                id: task.facts.operator_id.clone().unwrap_or_default(),
                label: task.facts.operator_label.clone().unwrap_or_default(),
            },
            source: TaskInputSource::Altitude,
            body: note,
            external_id: None,
        };
        self.record(
            &task.id,
            &[
                TaskHistoryEvent::TaskInputExpressed { input },
                TaskHistoryEvent::Steer,
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn unresolved_signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskAsk>, TaskServiceError> {
        self.store
            .unresolved_signal_threads(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn signal_threads(&self, task_id: &str) -> Result<Vec<TaskAsk>, TaskServiceError> {
        self.store
            .signal_threads(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn resolve_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        resolved: bool,
    ) -> Result<Task, TaskServiceError> {
        self.store
            .resolve_signal(task_id, signal_id, resolved, TaskAskAuthor::Operator, None)
            .await
            .map_err(TaskServiceError::Task)
    }

    // Closing a thread is the operator's word like any other, and a word wakes the
    // task it was waiting on: leaving a task paused behind a question nobody is asking
    // any more is the same silence as a question that was never heard.
    pub async fn close_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        resolved: bool,
    ) -> Result<(), TaskServiceError> {
        let thread = self
            .store
            .signal_thread(task_id, signal_id)
            .await
            .map_err(TaskServiceError::Task)?;
        let task = self.resolve_signal(task_id, signal_id, resolved).await?;
        let was_waiting = thread.as_ref().is_some_and(TaskAsk::waits_on_operator);
        if !resolved
            || !was_waiting
            || task.facts.waits_on_operator()
            || task.facts.accepted
            || task.facts.execution_pause_reason != Some(TaskExecutionPauseReason::AwaitingOperator)
        {
            return Ok(());
        }
        let title = thread.map(|thread| thread.title).unwrap_or_default();
        let prompt = format!(
            "The operator closed the thread \"{title}\", so it needs nothing more from them. Read the task, carry on with the work, and say what you are doing in the thread if it is still worth saying."
        );
        self.queue_wake(&task, prompt).await
    }

    pub async fn reply_operator_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        reply: String,
    ) -> Result<(), TaskServiceError> {
        // Re-read rather than trusting a caller-held task: the operator may have
        // opened the signal list some time before replying.
        let task = self
            .store
            .message_operator_signal(task_id, signal_id, &reply)
            .await
            .map_err(TaskServiceError::Task)?;
        if task.facts.accepted
            || task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer)
        {
            return Ok(());
        }
        let awake = matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Running | TaskExecutionStatus::Pending
        );
        let taken = if awake {
            TaskHistoryEvent::Steer
        } else {
            TaskHistoryEvent::Wake
        };
        let task = self.record(&task.id, &[taken]).await?;
        if awake {
            return Ok(());
        }
        // The turn reads the whole task and the thread; it does not receive a
        // pre-digested command.
        let prompt = format!(
            "The operator replied on signal {signal_id}. Read the thread and the task, answer in the thread, and do the work the reply asks for. Continue autonomously if there is more to do."
        );
        self.queue_wake(&task, prompt).await
    }

    // Every way a person or a pull request puts a task back to work ends here: the
    // task is queued with what woke it, and admission decides when the machine has
    // room for its sandbox. A wake is never the thing that makes a sandbox.
    async fn queue_wake(&self, task: &Task, prompt: String) -> Result<(), TaskServiceError> {
        self.record(
            &task.id,
            &[TaskHistoryEvent::ExecutionQueued {
                execution_status: TaskExecutionStatus::Pending,
                wake: Some(prompt),
            }],
        )
        .await?;
        Ok(())
    }

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

fn render_inputs(inputs: &[TaskInput]) -> String {
    let mut text = String::new();
    for input in inputs {
        let source = match &input.source {
            TaskInputSource::TaskCreated => "task created".to_owned(),
            TaskInputSource::Altitude | TaskInputSource::Notice => "altitude".to_owned(),
            TaskInputSource::TrackerComment { provider, .. } => {
                format!("{} comment", provider.label())
            }
        };
        text.push_str(&format!(
            "- [{} · {source}] {}\n",
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
