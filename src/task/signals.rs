//! What a task tells its operator, and what the operator answers.
//!
//! Signals are conversations. A reply is a message, not a typed command: the
//! agent reads it and decides what it means. Resolving a thread is the one
//! explicit state. Only accepting a review request also ends the task.

use crate::isolation::{CommandSession, IsolationEngine};

use super::{
    Task, TaskAgentRun, TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent,
    TaskOperatorSignalAuthor, TaskOperatorSignalThread, TaskService, TaskServiceError,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
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

    pub async fn operator_signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignalThread>, TaskServiceError> {
        self.store
            .operator_signal_threads(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn signal_threads(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignalThread>, TaskServiceError> {
        self.store
            .signal_threads(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    /// Resolve every update log the operator has now seen. Called when the
    /// signals screen is opened; the history remains under the resolved filter.
    pub async fn resolve_info_signals(&self, task_id: &str) -> Result<Task, TaskServiceError> {
        self.store
            .resolve_info_signals(task_id)
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
                let already_confirmed = self.store.events(&task.id).await.map_err(TaskServiceError::Task)?.iter()
                    .any(|(_, event)| matches!(event, TaskHistoryEvent::OperatorSignalConfirmed { signal_id: confirmed } if confirmed == signal_id));
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
