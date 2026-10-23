//! A task's life: founding it, provisioning it, and taking it apart.

use crate::{
    agent::{AgentKind, mcp},
    config::InstallationIdentity,
    isolation::Environment,
    isolation::{CommandEvent, CommandSession, IsolationEngine, SandboxOptions, VsockRoute},
    mounts, vcs,
};
use futures_util::future::join_all;

use crate::isolation::Unwatched;
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
    pub(super) fn draft(
        &self,
        intent: String,
        environment: Environment,
        agent: AgentKind,
        conversation: Option<String>,
        requester: Option<InstallationIdentity>,
    ) -> TaskDraft {
        let requester = requester.unwrap_or_else(|| self.identity.clone());
        TaskDraft {
            intent,
            environment,
            identity_id: requester.id,
            identity_label: requester.label,
            host: self.identity.label.clone(),
            agent_kind: Some(agent),
            agent_conversation_id: conversation
                .or_else(|| Some(self.agents.engine(agent).new_conversation_id())),
        }
    }

    pub(super) async fn adopt(
        &self,
        draft: TaskDraft,
        history: Option<&[(i64, TaskHistoryEvent)]>,
        arrival: &[TaskHistoryEvent],
    ) -> Result<Task, TaskServiceError> {
        self.store
            .create(draft, history, arrival)
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
        self.adopt(
            self.draft(intent, environment, agent, None, requester),
            None,
            &[],
        )
        .await
    }

    /// A copy of this service whose isolation builds for one operator.
    ///
    /// Nothing else changes: the store, the machine's identity and the progress
    /// registry belong to the machine, and only the directories a build writes
    /// into depend on who the work belongs to.
    pub(super) fn as_operator(&self, owner: &str) -> Result<Self, TaskServiceError> {
        Ok(Self {
            isolation: self
                .isolation
                .for_operator(owner)
                .map_err(TaskServiceError::Isolation)?,
            ..self.clone()
        })
    }

    /// Build what a task's environment needs, and say where its snapshot is.
    ///
    /// Local tools and profile mounts must be ready before a task arrives. Physical
    /// disk dependencies are synchronized separately against their exact snapshot.
    pub async fn prepare_environment(
        &self,
        environment: &Environment,
        agent: AgentKind,
    ) -> Result<PathBuf, TaskServiceError> {
        self.isolation
            .ensure_environment(
                environment.clone(),
                mounts::task(&self.agents, agent),
                &Unwatched,
            )
            .await
            .map(|snapshot| snapshot.path)
            .map_err(TaskServiceError::Isolation)
    }

    /// Build what a task's environment needs for one operator.
    pub async fn prepare_environment_as_operator(
        &self,
        owner: &str,
        environment: &Environment,
        agent: AgentKind,
    ) -> Result<PathBuf, TaskServiceError> {
        self.as_operator(owner)?
            .prepare_environment(environment, agent)
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
                // A task the host woke because a linked pull request needed
                // attention carries why. Anything else is a paused capsule coming
                // back, whose last turn may be days old by now, so the agent is
                // told what happened to it rather than simply asked to resume.
                let prompt = task.facts.review_wake_reason.clone().unwrap_or_else(|| {
                    if resume {
                        "Altitude paused this task and restored its capsule from a snapshot. You \
                         are continuing the same conversation, and the current time is in the \
                         context below. Review the conversation and workspace state, then continue \
                         the task where it left off."
                            .to_owned()
                    } else {
                        task.facts.intent.clone()
                    }
                });
                engine.autopilot_command(
                    &conversation_id,
                    &prompt,
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
        if task.facts.accepted {
            return Err(TaskServiceError::Task(TaskError::OperationRejected(
                "this task has already been accepted",
            )));
        }
        if task.facts.sandbox_available {
            return Ok(task);
        }
        let owner =
            task.operator_id()
                .map(|owner| owner.to_owned())
                .ok_or(TaskServiceError::Task(TaskError::OperationRejected(
                    "the task records no operator to build for",
                )))?;
        let isolation = self
            .isolation
            .for_operator(&owner)
            .map_err(TaskServiceError::Isolation)?;
        let sandbox_id = format!("altitude-{}", Uuid::new_v4());
        let snapshot_path = if task.facts.snapshot_available {
            task.facts.snapshot_path.clone()
        } else {
            None
        };
        let progress = self.environment_progress.writer(&task.id);
        // The agent's channel belongs to the sandbox: opened before it is made,
        // because a vsock route is fixed at creation and does not survive a
        // restored capsule's rebuild without being asked for again.
        let vsock = match task.agent_kind {
            Some(_) => vec![VsockRoute {
                host_socket: self.bind_agent_channel(&task.id)?,
                guest_port: mcp::AGENT_CHANNEL_PORT,
            }],
            None => Vec::new(),
        };
        let create = match task.agent_kind {
            Some(kind) => {
                isolation
                    .provision(
                        sandbox_id.clone(),
                        snapshot_path.clone(),
                        task.environment(),
                        [
                            vec![self.agents.engine(kind).environment_recipe()],
                            vcs::environment_recipes(),
                        ]
                        .concat(),
                        vsock,
                        &progress,
                    )
                    .await
            }
            None => {
                isolation
                    .create(sandbox_id.clone(), SandboxOptions::isolated(snapshot_path))
                    .await
            }
        };
        self.environment_progress.clear(&task.id);
        if let Err(error) = create {
            self.release_agent_channel(&task.id);
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
            Ok(task) => {
                if self
                    .store
                    .is_active(&task.id)
                    .await
                    .map_err(TaskServiceError::Task)?
                {
                    Ok(task)
                } else {
                    self.release_resources(&task).await?;
                    Err(TaskServiceError::Task(TaskError::OperationRejected(
                        "the task was cancelled during preparation",
                    )))
                }
            }
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
        // Record cancellation before cleanup so a concurrent provision cannot resume the work.
        self.store
            .remove(&task.id)
            .await
            .map_err(TaskServiceError::Task)?;
        let task = self
            .store
            .get(&task.id)
            .await
            .map_err(TaskServiceError::Task)?
            .unwrap_or(task);
        self.release_resources(&task).await?;
        Ok(())
    }
    pub(super) async fn release_resources(&self, task: &Task) -> Result<Task, TaskServiceError> {
        self.release_agent_channel(&task.id);
        let mut task = task.clone();
        if task.facts.sandbox_available {
            let id = task
                .facts
                .sandbox_id
                .clone()
                .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
            if let Err(error) = self.isolation.remove(id).await {
                if !task.facts.accepted {
                    self.fail(&task, error.to_string()).await?;
                }
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
                if !task.facts.accepted {
                    self.fail(&task, error.to_string()).await?;
                }
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
                            service.release_agent_channel(&task.id);
                            let snapshot_path = path.to_string_lossy().into_owned();
                            // A pause recovery was interrupted mid-way through, so
                            // finish it rather than turning it into a fresh turn.
                            // A held review is one of those pauses.
                            let event = if task.facts.deferred_review.is_some() {
                                TaskHistoryEvent::ExecutionPaused {
                                    execution_status: TaskExecutionStatus::Paused,
                                    sandbox_available: false,
                                    snapshot_available: true,
                                    execution_pause_reason:
                                        TaskExecutionPauseReason::AwaitingReviewChecks,
                                    snapshot_path,
                                }
                            } else if task.facts.awaiting_signal_kind.is_some() {
                                TaskHistoryEvent::ExecutionPaused {
                                    execution_status: TaskExecutionStatus::Paused,
                                    sandbox_available: false,
                                    snapshot_available: true,
                                    execution_pause_reason:
                                        TaskExecutionPauseReason::AwaitingOperator,
                                    snapshot_path,
                                }
                            } else {
                                TaskHistoryEvent::ExecutionRecovered {
                                    execution_status: TaskExecutionStatus::Pending,
                                    sandbox_available: false,
                                    snapshot_available: true,
                                    snapshot_path,
                                }
                            };
                            task = service.record_snapshot(&task, &[event]).await?;
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
                } else if task.facts.deferred_review.is_some() && task.facts.snapshot_available {
                    // A shutdown can catch a review between the hold being
                    // recorded and its capsule being suspended, which leaves it
                    // paused for the shutdown — a pause recovery resumes.
                    task = service
                        .record(
                            &task.id,
                            &[TaskHistoryEvent::ExecutionPaused {
                                execution_status: TaskExecutionStatus::Paused,
                                sandbox_available: false,
                                snapshot_available: true,
                                execution_pause_reason:
                                    TaskExecutionPauseReason::AwaitingReviewChecks,
                                snapshot_path: task.facts.snapshot_path.clone().unwrap_or_default(),
                            }],
                        )
                        .await?;
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
                        service.release_agent_channel(&task.id);
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
