//! A task's life: founding it, provisioning it, and taking it apart.

use crate::{
    agent::AgentKind,
    config::InstallationIdentity,
    isolation::Environment,
    isolation::{CommandEvent, CommandSession, IsolationEngine, SandboxOptions},
    vcs::GitHub,
};
use futures_util::future::join_all;
use std::path::PathBuf;
use uuid::Uuid;

use super::{
    Task, TaskAgentRun, TaskAgentRunOutcome, TaskDraft, TaskError, TaskExecutionMode,
    TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent, TaskService, TaskServiceError,
    failure_events, run_ended, should_resume, should_suspend, snapshot_name,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    /// Start a task, driven by `agent` in `environment`.
    ///
    /// The two are independent: the environment says what is installed, and the
    /// agent says what works in it.
    ///
    /// `requester` is whose work this is. It is the machine's own installation unless
    /// a client asked for the task, in which case it is the client's: a delegated
    /// task is the operator's work on somebody else's machine, and its record should
    /// say so.
    /// Found a task here, continuing a conversation it was already having.
    ///
    /// A task taken over from another machine was in the middle of a conversation, and
    /// the agent should pick it up rather than meet the work for the first time. With
    /// no conversation given, this is a task starting fresh.
    pub async fn adopt(
        &self,
        intent: String,
        environment: Environment,
        agent: AgentKind,
        conversation: Option<String>,
        requester: Option<InstallationIdentity>,
    ) -> Result<Task, TaskServiceError> {
        let requester = requester.unwrap_or_else(|| self.identity.clone());
        self.store
            .create(TaskDraft {
                intent,
                environment,
                identity_id: requester.id.clone(),
                identity_label: requester.label.clone(),
                // The machine that runs it, which is this one. A task belongs to the
                // machine running it and to nothing else.
                host: self.identity.label.clone(),
                agent_kind: Some(agent),
                agent_conversation_id: conversation
                    .or_else(|| Some(self.agents.engine(agent).new_conversation_id())),
            })
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn create(
        &self,
        intent: String,
        environment: Environment,
        agent: AgentKind,
        requester: Option<InstallationIdentity>,
    ) -> Result<Task, TaskServiceError> {
        self.adopt(intent, environment, agent, None, requester)
            .await
    }

    /// Append history that happened on another machine, at the times it happened.
    pub async fn record_borrowed_history(
        &self,
        task_id: &str,
        events: &[(i64, TaskHistoryEvent)],
    ) -> Result<Task, TaskServiceError> {
        self.store
            .append_borrowed_events(task_id, events)
            .await
            .map_err(TaskServiceError::Task)
    }

    /// Record that this task's capsule is here, and where it is.
    ///
    /// The capsule arrived from another machine, and this is what makes it this
    /// machine's: the path it landed at is the disk the task will run from.
    pub async fn record_capsule(
        &self,
        task_id: &str,
        path: &str,
    ) -> Result<Task, TaskServiceError> {
        self.record(
            task_id,
            &[TaskHistoryEvent::ExecutionRecovered {
                execution_status: TaskExecutionStatus::Pending,
                sandbox_available: false,
                snapshot_available: true,
                snapshot_path: path.to_owned(),
            }],
        )
        .await
    }
    pub(super) async fn record(
        &self,
        task_id: &str,
        events: &[TaskHistoryEvent],
    ) -> Result<Task, TaskServiceError> {
        self.store
            .append_events(task_id, events)
            .await
            .map_err(TaskServiceError::Task)
    }
    pub async fn start(
        &self,
        task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        let resume = task.facts.snapshot_available;
        let task = self.provision(task).await?;
        let command = match task.agent_kind {
            Some(kind) => {
                let engine = self.agents.engine(kind);
                let conversation_id = self.agent_conversation(&task, resume).await?;
                engine.autopilot_command(
                    &conversation_id,
                    if resume {
                        "Resume the task and continue making progress."
                    } else {
                        &task.facts.intent
                    },
                    &task.turn_context(TaskExecutionMode::Autopilot),
                    resume,
                )
            }
            None => task.facts.intent.clone(),
        };
        self.launch(task, command, TaskExecutionMode::Autopilot)
            .await
    }
    pub(super) async fn provision(&self, task: Task) -> Result<Task, TaskServiceError> {
        let sandbox_id = format!("altitude-{}", Uuid::new_v4());
        let snapshot_path = if task.facts.snapshot_available {
            task.facts.snapshot_path.clone()
        } else {
            None
        };
        let progress = self.environment_progress.writer(&task.id);
        let create = match task.agent_kind {
            Some(kind) => {
                self.isolation
                    .provision(
                        sandbox_id.clone(),
                        snapshot_path.clone(),
                        task.environment(),
                        vec![
                            self.agents.engine(kind).environment_recipe(),
                            GitHub::environment_recipe(),
                        ],
                        &progress,
                    )
                    .await
            }
            None => {
                self.isolation
                    .create(sandbox_id.clone(), SandboxOptions::isolated(snapshot_path))
                    .await
            }
        };
        self.environment_progress.clear(&task.id);
        if let Err(error) = create {
            self.fail(&task, error.to_string()).await?;
            return Err(TaskServiceError::Isolation(error));
        }
        match self
            .record(
                &task.id,
                &[TaskHistoryEvent::SandboxProvisioned {
                    sandbox_available: true,
                    sandbox_id: sandbox_id.clone(),
                }],
            )
            .await
        {
            Ok(task) => Ok(task),
            Err(error) => {
                if let Err(cleanup_error) = self.isolation.remove(sandbox_id).await {
                    tracing::warn!(%cleanup_error, "could not remove unrecorded task sandbox");
                }
                Err(error)
            }
        }
    }
    pub async fn finish(
        &self,
        task: &Task,
        agent_run: &TaskAgentRun,
        event: CommandEvent,
        diagnostic: Option<&str>,
    ) -> Result<Task, TaskServiceError> {
        let (exit_code, error) = match event {
            // A clean exit is not an ending: a run that stops without a report has
            // failed to report, and only the operator closes a task out.
            CommandEvent::Exited(0) => (Some(0), "the run ended without a task report".to_owned()),
            CommandEvent::Exited(code) => (Some(code), format!("command exited with code {code}")),
            CommandEvent::Failed(error) => (None, error),
            CommandEvent::Output { .. } => unreachable!("output is handled by the event_loop"),
        };
        let error = diagnostic.map_or(error.clone(), |detail| format!("{error}: {detail}"));
        let mut events = vec![run_ended(agent_run, TaskAgentRunOutcome::Failed, exit_code)];
        events.extend(failure_events(task, error));
        self.record(&task.id, &events).await
    }
    pub async fn remove(&self, task: Task) -> Result<(), TaskServiceError> {
        self.release_resources(&task).await?;
        self.store
            .remove(&task.id)
            .await
            .map_err(TaskServiceError::Task)
    }
    pub(super) async fn release_resources(&self, task: &Task) -> Result<Task, TaskServiceError> {
        let mut task = task.clone();
        if task.facts.sandbox_available {
            let id = task
                .facts
                .sandbox_id
                .clone()
                .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
            if let Err(error) = self.isolation.remove(id).await {
                self.fail(&task, error.to_string()).await?;
                return Err(TaskServiceError::Isolation(error));
            }
            task = self
                .record(
                    &task.id,
                    &[TaskHistoryEvent::SandboxRemoved {
                        sandbox_available: false,
                    }],
                )
                .await?;
        }
        if task.facts.snapshot_available {
            let path = task
                .facts
                .snapshot_path
                .as_ref()
                .expect("available snapshot has a recorded path");
            if let Err(error) = self.isolation.remove_snapshot(PathBuf::from(path)).await {
                self.fail(&task, error.to_string()).await?;
                return Err(TaskServiceError::Isolation(error));
            }
            task = self
                .record(
                    &task.id,
                    &[TaskHistoryEvent::SnapshotRemoved {
                        snapshot_available: false,
                    }],
                )
                .await?;
        }
        Ok(task)
    }
    pub async fn recover_all(&self) -> Vec<Result<Task, TaskServiceError>> {
        let tasks = match self.list().await {
            Ok(tasks) => tasks,
            Err(error) => return vec![Err(error)],
        };
        join_all(tasks.into_iter().filter(should_resume).map(|task| {
            let service = self.clone();
            async move {
                let mut task = task;
                if let Some(sandbox_id) = &task.facts.sandbox_id
                    && task.facts.sandbox_available
                {
                    match service
                        .isolation
                        .suspend(
                            sandbox_id.clone(),
                            snapshot_name(&task.id),
                            service.snapshots_dir.clone(),
                        )
                        .await
                    {
                        Ok(path) => {
                            task = service
                                .record_snapshot(
                                    &task,
                                    &[TaskHistoryEvent::ExecutionRecovered {
                                        execution_status: TaskExecutionStatus::Pending,
                                        sandbox_available: false,
                                        snapshot_available: true,
                                        snapshot_path: path.to_string_lossy().into_owned(),
                                    }],
                                )
                                .await?;
                        }
                        Err(error) => {
                            service
                                .fail(
                                    &task,
                                    format!("could not recover the task sandbox: {error}"),
                                )
                                .await?;
                            return Err(TaskServiceError::Isolation(error));
                        }
                    }
                }
                Ok(task)
            }
        }))
        .await
    }
    /// Everything that happened to a task, in order, with when it happened.
    pub async fn events(
        &self,
        task_id: &str,
    ) -> Result<Vec<(i64, TaskHistoryEvent)>, TaskServiceError> {
        self.store
            .events(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn suspend_all(&self, tasks: Vec<Task>) -> Vec<Result<(), TaskServiceError>> {
        join_all(tasks.into_iter().filter(should_suspend).map(|task| {
            let service = self.clone();
            async move {
                let sandbox_id = task
                    .facts
                    .sandbox_id
                    .clone()
                    .expect("suspendable tasks have a sandbox");
                match service
                    .isolation
                    .suspend(
                        sandbox_id,
                        snapshot_name(&task.id),
                        service.snapshots_dir.clone(),
                    )
                    .await
                {
                    Ok(path) => {
                        let snapshot_path = path.to_string_lossy().into_owned();
                        let event = if task.facts.execution_status == TaskExecutionStatus::Failed {
                            TaskHistoryEvent::TaskSnapshotSaved {
                                snapshot_path,
                                sandbox_available: false,
                                snapshot_available: true,
                            }
                        } else {
                            TaskHistoryEvent::ExecutionPaused {
                                execution_status: TaskExecutionStatus::Paused,
                                sandbox_available: false,
                                snapshot_available: true,
                                execution_pause_reason:
                                    TaskExecutionPauseReason::ApplicationShutdown,
                                snapshot_path,
                            }
                        };
                        service.record_snapshot(&task, &[event]).await?;
                        Ok(())
                    }
                    Err(error) => {
                        service
                            .fail(&task, format!("could not save the task sandbox: {error}"))
                            .await?;
                        Err(TaskServiceError::Isolation(error))
                    }
                }
            }
        }))
        .await
    }
    pub(super) async fn record_snapshot(
        &self,
        task: &Task,
        events: &[TaskHistoryEvent],
    ) -> Result<Task, TaskServiceError> {
        let updated = self.record(&task.id, events).await?;
        if let Some(previous) = &task.facts.snapshot_path
            && task.facts.snapshot_available
            && updated.facts.snapshot_path.as_ref() != Some(previous)
            && let Err(error) = self
                .isolation
                .remove_snapshot(PathBuf::from(previous))
                .await
        {
            tracing::warn!(task_id = %task.id, %error, "could not remove superseded task snapshot");
        }
        Ok(updated)
    }
    pub(super) async fn fail(&self, task: &Task, error: String) -> Result<Task, TaskServiceError> {
        tracing::error!(task_id = %task.id, %error, "task execution failed");
        self.record(&task.id, &failure_events(task, error)).await
    }
}
