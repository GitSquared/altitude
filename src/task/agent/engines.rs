use crate::{
    agent::{AgentEngine, AgentKind, TaskProgressDecoder, TaskStreamDecoder},
    isolation::IsolationEngine,
};

use crate::task::{
    AgentError, Task, TaskAgentTurnContext, TaskError, TaskExecutionMode, TaskService,
    TaskServiceError,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub(crate) async fn agent_conversation(
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
    // What a turn is told about this task. The login belongs to the project's
    // credential rather than to the task, so it is read here, where the credential
    // store is, and not from the record.
    pub(crate) fn turn_context(
        &self,
        task: &Task,
        mode: TaskExecutionMode,
    ) -> TaskAgentTurnContext {
        let mut context = task.turn_context(mode);
        context.vcs_identity = self.vcs_identity(task);
        context
    }

    fn vcs_identity(&self, task: &Task) -> Option<String> {
        let credential = task.facts.project.as_ref()?.vcs_credential.as_deref()?;
        let settings = self.credentials.config().settings_if_written()?;
        let label = settings.credential(credential)?.label.trim().to_owned();
        (!label.is_empty()).then_some(label)
    }

    pub(crate) fn engine(&self, task: &Task) -> Result<&dyn AgentEngine, TaskServiceError> {
        Ok(self.agents.engine(self.agent_kind(task)?))
    }
    pub(crate) fn agent_kind(&self, task: &Task) -> Result<AgentKind, TaskServiceError> {
        task.agent_kind
            .ok_or(TaskServiceError::Task(TaskError::MissingAgent))
    }
    pub fn progress_decoder(&self, task: &Task) -> Option<Box<dyn TaskProgressDecoder>> {
        self.agent_kind(task)
            .ok()
            .and_then(|kind| self.agents.engine(kind).progress_decoder())
    }

    pub fn stream_decoder(&self, task: &Task) -> Option<Box<dyn TaskStreamDecoder>> {
        task.agent_kind
            .map(|kind| self.agents.engine(kind).stream_decoder(&task.facts.usage))
    }
    pub(crate) async fn conversation_failure<T>(
        &self,
        task: &Task,
        message: impl Into<String>,
    ) -> Result<T, TaskServiceError> {
        let error = AgentError::new(message);
        self.fail(task, error.to_string()).await?;
        Err(TaskServiceError::Agent(error))
    }
}
