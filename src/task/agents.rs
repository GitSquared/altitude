//! How a service talks to a harness: which engine drives a task, and the
//! conversation it is driving.

use crate::{
    agent::{AgentEngine, AgentKind, TaskReportDecoder},
    isolation::IsolationEngine,
};

use super::{AgentError, Task, TaskError, TaskService, TaskServiceError};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    /// The task's conversation in its harness, after confirming that the
    /// harness can still resume it.
    pub(super) async fn agent_conversation(
        &self,
        task: &Task,
        require_transcript: bool,
    ) -> Result<String, TaskServiceError> {
        let kind = self.agent_kind(task)?;
        let Some(conversation_id) = task.facts.agent_conversation_id.as_deref() else {
            return self
                .conversation_failure(task, format!("{kind} task has no conversation ID"))
                .await;
        };
        if !require_transcript {
            return Ok(conversation_id.to_owned());
        }
        let transcript_path = self
            .agents
            .engine(kind)
            .conversation_transcript_path(conversation_id);
        let sandbox_id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        match self.isolation.guest_regular_file_exists(sandbox_id, transcript_path).await {
            Ok(true) => Ok(conversation_id.to_owned()),
            Ok(false) => self.conversation_failure(task, format!("{kind} conversation transcript is missing; the task cannot resume without its session state")).await,
            Err(error) => self.conversation_failure(task, format!("could not read {kind} conversation transcript: {error}")).await,
        }
    }
    pub(super) fn engine(&self, task: &Task) -> Result<&dyn AgentEngine, TaskServiceError> {
        Ok(self.agents.engine(self.agent_kind(task)?))
    }
    pub(super) fn agent_kind(&self, task: &Task) -> Result<AgentKind, TaskServiceError> {
        task.agent_kind
            .ok_or(TaskServiceError::Task(TaskError::MissingAgent))
    }
    pub fn report_decoder(&self, task: &Task) -> Option<Box<dyn TaskReportDecoder>> {
        task.agent_kind
            .map(|kind| self.agents.engine(kind).report_decoder())
    }
    pub(super) async fn conversation_failure<T>(
        &self,
        task: &Task,
        message: impl Into<String>,
    ) -> Result<T, TaskServiceError> {
        let error = AgentError::new(message);
        self.fail(task, error.to_string()).await?;
        Err(TaskServiceError::Agent(error))
    }
}
