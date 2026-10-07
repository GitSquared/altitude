//! What a task tells its operator, and what the operator answers.

use crate::isolation::{CommandSession, IsolationEngine};

use super::{
    Task, TaskAgentRun, TaskExecutionStatus, TaskHistoryEvent, TaskOperatorSignal, TaskService,
    TaskServiceError,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    pub async fn operator_signals(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskOperatorSignal>, TaskServiceError> {
        self.store
            .operator_signals(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }
    pub async fn read_operator_signals(&self, task_id: &str) -> Result<Task, TaskServiceError> {
        self.store
            .read_operator_signals(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }
    pub async fn acknowledge_operator_signal(&self, id: &str) -> Result<Task, TaskServiceError> {
        self.store
            .acknowledge_operator_signal(id)
            .await
            .map_err(TaskServiceError::Task)
    }
    pub async fn answer_operator_signal(
        &self,
        signal_id: &str,
        answer: String,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        // Re-read rather than trusting a caller-held task: the operator may have
        // opened the signal list some time before answering.
        let mut task = self
            .store
            .answer_operator_signal(signal_id, &answer)
            .await
            .map_err(TaskServiceError::Task)?;
        if !task.facts.sandbox_available {
            task = self.provision(task).await?;
        }
        self.start_autopilot_turn(
            task,
            &format!(
                "The operator answered attention signal {signal_id}:\n\n{answer}\n\nApply this new intent and continue autonomously."
            ),
        )
        .await
    }
    /// Accept a wrap-up request: the operator's judgment, and the only ending.
    ///
    /// The judgment is recorded before the capsule is touched, so what the
    /// operator decided does not depend on cleanup succeeding. Nothing here ends a
    /// run: the run that asked for review already ended, waiting for this answer.
    pub async fn confirm_operator_signal(&self, signal_id: &str) -> Result<Task, TaskServiceError> {
        let task = self
            .store
            .confirm_operator_signal(signal_id)
            .await
            .map_err(TaskServiceError::Task)?;
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
