//! Starting a service, and the questions an interface asks of it.

use crate::{
    agent::{AgentEngine, AgentEngines, AgentError, AgentKind, TaskReportDecoder},
    config::InstallationIdentity,
    isolation::{
        EnvironmentDescription, EnvironmentProgress, IsolationEngine, ProfileMount, ProfileSummary,
        guest_path,
    },
    vcs::GitHub,
};
use std::{collections::HashMap, path::PathBuf};

use super::*;

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    pub fn new(
        store: TaskStore,
        isolation: E,
        snapshots_dir: PathBuf,
        agents: AgentEngines,
        identity: InstallationIdentity,
    ) -> Self {
        Self {
            store,
            isolation,
            snapshots_dir,
            agents,
            identity,
            environment_progress: EnvironmentProgressRegistry::default(),
        }
    }

    // The identity every task this installation founds is recorded under.
    /// What each task's environment build is doing, for an interface to show.
    ///
    /// Keyed by task, and only present while this service is building for it.
    pub fn environment_builds(&self) -> HashMap<String, EnvironmentProgress> {
        self.environment_progress.published()
    }
    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.store.list().await.map_err(TaskServiceError::Task)
    }
    /// Bring back what was running, and return the tasks that want a slot.
    ///
    /// Recovery is not starting. This puts a task's work back on disk and leaves
    /// it waiting, so the caller decides what the host can carry; starting here
    /// would run everything at once on the way up.
    /// Every profile a task mounts, so an interface can say what logins exist.
    ///
    /// Built from the same recipes a task is provisioned with, so this cannot
    /// drift from what is actually mounted.
    pub fn profiles(&self) -> Vec<ProfileSummary> {
        let recipes = [
            self.agents.engine(AgentKind::Pi).environment_recipe(),
            self.agents.engine(AgentKind::Claude).environment_recipe(),
            GitHub::environment_recipe(),
        ];
        recipes
            .into_iter()
            .map(|recipe| ProfileSummary {
                profile: recipe.profile,
                mounts: recipe
                    .directories
                    .iter()
                    .map(|directory| ProfileMount {
                        path: directory.relative_path,
                        guest: guest_path(&directory.guest_path),
                    })
                    .collect(),
            })
            .collect()
    }
    /// What the workspace base says about itself, for an interface that
    /// describes environments rather than runs them.
    pub fn base_description(&self) -> Option<EnvironmentDescription> {
        self.isolation.base_description()
    }
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
    /// The harness that owns this task's conversation.
    pub(super) fn engine(&self, task: &Task) -> Result<&dyn AgentEngine, TaskServiceError> {
        Ok(self.agents.engine(self.agent_kind(task)?))
    }
    pub(super) fn agent_kind(&self, task: &Task) -> Result<AgentKind, TaskServiceError> {
        task.agent_kind
            .ok_or(TaskServiceError::Task(TaskError::MissingAgent))
    }
    /// Decodes a report turn's output for the task's harness.
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
