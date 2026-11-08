use crate::isolation::{CommandSession, IsolationEngine};

use crate::task::{
    Task, TaskAgentRun, TaskAsk, TaskAskAuthor, TaskError, TaskExecutionPauseReason,
    TaskExecutionStatus, TaskHistoryEvent, TaskInput, TaskInputAuthor, TaskInputSource,
    TaskService, TaskServiceError,
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
        let task = self
            .record(&task.id, &[TaskHistoryEvent::TaskInputExpressed { input }])
            .await?;
        if matches!(
            task.facts.execution_status,
            TaskExecutionStatus::Running | TaskExecutionStatus::Pending
        ) {
            return Ok(None);
        }
        let intends = self.inputs(&task.id).await?;
        let prompt = format!(
            "The operator sent a new message. Read it and the task, answer through a signal              if needed, and continue autonomously.\n\nExpressed intent so far:\n{}",
            render_inputs(&intends)
        );
        let (task, run, session) = self.start_autopilot_turn(task, &prompt, None).await?;
        Ok(Some((task, run, session)))
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
            .resolve_signal(task_id, signal_id, resolved, TaskAskAuthor::Operator)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn reply_operator_signal(
        &self,
        task_id: &str,
        signal_id: &str,
        reply: String,
    ) -> Result<Option<(Task, TaskAgentRun, CommandSession)>, TaskServiceError> {
        // Re-read rather than trusting a caller-held task: the operator may have
        // opened the signal list some time before replying.
        let task = self
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
            TaskInputSource::Altitude | TaskInputSource::Notice => "Altitude".to_owned(),
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
