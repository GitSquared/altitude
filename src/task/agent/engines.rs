use crate::{
    agent::{AgentEngine, AgentKind, TaskProgressDecoder, TaskStreamDecoder},
    isolation::IsolationEngine,
};

use std::path::PathBuf;

use crate::task::memory;
use crate::task::{
    AgentError, ProjectMemoryFile, Task, TaskAgentTurnContext, TaskError, TaskExecutionMode,
    TaskService, TaskServiceError,
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

    pub(crate) async fn project_memory_state(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProjectMemoryFile>, TaskServiceError> {
        self.store
            .project_memory_state(project_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    // The directory the sandbox mounts for this project, for the operator whose work
    // it is: the engine scopes it, so the sync writes where the agent reads.
    fn memory_directory(&self, owner: Option<&str>, project_id: &str) -> PathBuf {
        owner
            .and_then(|owner| self.isolation.for_operator(owner).ok())
            .map(|isolation| isolation.project_memory_dir(project_id))
            .unwrap_or_else(|| self.isolation.project_memory_dir(project_id))
    }

    // Record what a project's memory directory now holds, as events on the task that
    // is here to carry them, so the directory's changes have a log to travel by.
    pub(crate) async fn capture_project_memory(
        &self,
        owner: Option<&str>,
        task_id: &str,
        project_id: &str,
    ) {
        let directory = self.memory_directory(owner, project_id);
        let on_disk = match memory::read(&directory) {
            Ok(files) => files,
            Err(error) => {
                tracing::warn!(%error, %project_id, "could not read a project's memory");
                return;
            }
        };
        let known = match self.project_memory_state(project_id).await {
            Ok(known) => known,
            Err(error) => {
                tracing::warn!(%error, %project_id, "could not read a project's memory state");
                return;
            }
        };
        let events = memory::changes(project_id, &on_disk, &known);
        if events.is_empty() {
            return;
        }
        if let Err(error) = self.store.append_events(task_id, &events).await {
            tracing::warn!(%error, %project_id, "could not record a project's memory");
        }
    }

    // Bring a project's memory directory up to what its newest events say, so a task
    // that just arrived finds what earlier tasks elsewhere wrote. A file newer on this
    // machine is left alone, so applying never costs work this machine did later.
    pub(crate) async fn apply_project_memory(&self, owner: Option<&str>, project_id: &str) {
        match self.project_memory_state(project_id).await {
            Ok(state) => {
                let directory = self.memory_directory(owner, project_id);
                if let Err(error) = memory::write(&directory, &state) {
                    tracing::warn!(%error, %project_id, "could not write a project's memory");
                }
            }
            Err(error) => {
                tracing::warn!(%error, %project_id, "could not read a project's memory state");
            }
        }
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
