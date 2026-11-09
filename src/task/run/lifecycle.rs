use crate::{
    agent::{AgentKind, mcp},
    config::InstallationIdentity,
    isolation::{
        CommandEvent, CommandSession, FileTransferSink, IsolationEngine, SandboxOptions,
        SandboxPurpose, VsockRoute, guest_absolute, mounts,
    },
    project::Project,
    vcs::{VcsProviderKind, now_ms},
};
use futures_util::future::join_all;

use crate::isolation::EnvironmentProgressSink;
use std::{fs, path::PathBuf, time::Instant};
use uuid::Uuid;

use crate::remote::{TRANSCRIPT_FILE, WORK_PLAIN_FILE};
use crate::task::{
    Task, TaskAgentRun, TaskAgentRunOutcome, TaskDraft, TaskError, TaskExecutionMode,
    TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent, TaskInput, TaskInputAuthor,
    TaskInputSource, TaskService, TaskServiceError, failure_events, run_ended, should_resume,
    should_suspend, snapshot_name,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub(crate) fn draft(
        &self,
        intent: String,
        project: Project,
        agent: AgentKind,
        conversation: Option<String>,
        requester: Option<InstallationIdentity>,
    ) -> TaskDraft {
        let requester = requester.unwrap_or_else(|| self.identity.clone());
        TaskDraft {
            intent,
            project,
            identity_id: requester.id,
            identity_label: requester.label,
            host: self.identity.label.clone(),
            agent_kind: Some(agent),
            agent_conversation_id: conversation
                .or_else(|| Some(self.agents.engine(agent).new_conversation_id())),
        }
    }

    pub(crate) async fn adopt(
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
        project: Project,
        agent: AgentKind,
        requester: Option<InstallationIdentity>,
    ) -> Result<Task, TaskServiceError> {
        self.adopt(
            self.draft(intent, project, agent, None, requester),
            None,
            &[],
        )
        .await
    }

    pub(crate) fn as_operator(&self, owner: &str) -> Result<Self, TaskServiceError> {
        Ok(Self {
            isolation: self
                .isolation
                .for_operator(owner)
                .map_err(TaskServiceError::Isolation)?,
            ..self.clone()
        })
    }

    // Where the minutes of a cold environment go. A caller with something to say
    // about them passes a sink; one with nothing to say passes `Unwatched`.
    pub async fn prepare_project(
        &self,
        project: &Project,
        agent: AgentKind,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<PathBuf, TaskServiceError> {
        self.isolation
            .ensure_project(project.clone(), mounts::task(&self.agents, agent), progress)
            .await
            .map(|snapshot| snapshot.path)
            .map_err(TaskServiceError::Isolation)
    }

    pub async fn prepare_project_as_operator(
        &self,
        owner: &str,
        project: &Project,
        agent: AgentKind,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<PathBuf, TaskServiceError> {
        self.as_operator(owner)?
            .prepare_project(project, agent, progress)
            .await
    }

    pub(crate) async fn record(
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
                let asking = Instant::now();
                let conversation_id = self.agent_conversation(&task, resume).await?;
                tracing::info!(
                    task = %task.id,
                    elapsed_ms = asking.elapsed().as_millis() as u64,
                    resumed = resume,
                    "the agent's conversation is ready"
                );
                // A task the host woke because a linked pull request needed
                // attention carries the news itself. Anything else is a paused
                // capsule coming back, whose last turn may be days old by now, so
                // the agent is told what happened to it rather than simply asked to
                // resume.
                let prompt = task
                    .facts
                    .review_wake_news
                    .as_ref()
                    .map(|news| format!("altitude checked the pull requests linked to this task:\n\n{news}"))
                    .unwrap_or_else(|| {
                    if resume {
                        "altitude paused this task and restored its sandbox from a snapshot. You \
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
                    &self.turn_context(&task, TaskExecutionMode::Autopilot),
                    resume,
                )
            }
            None => task.facts.intent.clone(),
        };
        self.launch(task, command, TaskExecutionMode::Autopilot, None)
            .await
    }
    pub(crate) fn arrivals_dir(&self, task_id: &str) -> PathBuf {
        self.credentials
            .config()
            .data_dir()
            .join("arrivals")
            .join(task_id)
    }

    pub(crate) async fn provision(&self, task: Task) -> Result<Task, TaskServiceError> {
        if task.facts.accepted {
            return Err(TaskServiceError::Task(TaskError::OperationRejected(
                "this task has already been accepted",
            )));
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
        // The record is not the runtime: a task whose capsule went away with a hard
        // stop would otherwise be provisioned for the sandbox it says it has, and
        // every later step would be reaching for something that is not there.
        let mut task = task;
        if task.facts.sandbox_available
            && let Some(sandbox_id) = task.facts.sandbox_id.clone()
            && !isolation.has_sandbox(sandbox_id).await
        {
            // This is the one place that knows a capsule is gone rather than
            // unparked, so it is where the agent is told its workspace went with it.
            let mut events = vec![TaskHistoryEvent::SandboxRemoved {
                sandbox_available: false,
            }];
            events.extend(notice_event(
                &task,
                "altitude lost this task's sandbox, so the workspace from earlier turns is \
                 gone. The project's repository is checked out fresh for you; recreate what \
                 you need and carry on with the work.",
            ));
            task = self.record(&task.id, &events).await?;
        }
        if task.facts.sandbox_available {
            return Ok(task);
        }
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
            Some(_) => vec![
                VsockRoute {
                    host_socket: self.bind_agent_channel(&task.id)?,
                    guest_port: mcp::AGENT_CHANNEL_PORT,
                },
                // Fixed now, bound when the operator opens a terminal: a task
                // started before a takeover is still reachable when it comes.
                self.relay_channel(&sandbox_id),
            ],
            None => Vec::new(),
        };
        let project = task
            .project()
            .cloned()
            .ok_or(TaskServiceError::Task(TaskError::MissingProject))?;
        // The longest wait in a start, and the one a wedged machine never gets past,
        // so it is bracketed: what a log shows between these two lines is the answer.
        let preparing = Instant::now();
        tracing::info!(
            task = %task.id,
            resumed = snapshot_path.is_some(),
            agent = task.agent_kind.is_some(),
            "preparing a task's sandbox"
        );
        let create = match task.agent_kind {
            Some(kind) => {
                let agent_id = project
                    .agent_credential
                    .clone()
                    .unwrap_or_else(|| kind.profile().to_owned());
                let vcs_id = project
                    .vcs_credential
                    .clone()
                    .or_else(|| project.vcs.map(VcsProviderKind::profile).map(str::to_owned))
                    .unwrap_or_else(|| VcsProviderKind::sole().profile().to_owned());
                let agent_recipe = self
                    .credentials
                    .recipe(&agent_id)
                    .map_err(TaskServiceError::Credentials)?;
                let vcs_recipe = self
                    .credentials
                    .recipe(&vcs_id)
                    .map_err(TaskServiceError::Credentials)?;
                isolation
                    .provision(
                        sandbox_id.clone(),
                        snapshot_path.clone(),
                        project,
                        vec![agent_recipe, vcs_recipe],
                        vsock,
                        &progress,
                    )
                    .await
            }
            None => {
                isolation
                    .create(
                        sandbox_id.clone(),
                        SandboxOptions::isolated(snapshot_path),
                        SandboxPurpose::Capsule,
                    )
                    .await
            }
        };
        self.environment_progress.clear(&task.id);
        tracing::info!(
            task = %task.id,
            elapsed_ms = preparing.elapsed().as_millis() as u64,
            ready = create.is_ok(),
            "a task's sandbox is ready"
        );
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
                    .is_kept(&task.id)
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
    pub(crate) async fn deliver_arrivals(
        &self,
        isolation: &E,
        task: &Task,
        sandbox_id: &str,
        progress: &dyn FileTransferSink,
    ) -> Result<(), TaskServiceError> {
        let arrivals = self.arrivals_dir(&task.id);
        if !arrivals.is_dir() {
            return Ok(());
        }
        let checkout = task
            .project()
            .and_then(|project| project.checkout_path())
            .and_then(|path| guest_absolute(&path).ok());
        let work = arrivals.join(WORK_PLAIN_FILE);
        if let Some(checkout) = checkout
            && work.is_file()
        {
            let tally = isolation
                .deliver_files(sandbox_id.to_owned(), work, checkout, progress)
                .await
                .map_err(TaskServiceError::Isolation)?;
            tracing::info!(
                task = %task.id,
                files = tally.files,
                bytes = tally.bytes,
                "put the files this task arrived with into its sandbox"
            );
        }
        let transcript = arrivals.join(TRANSCRIPT_FILE);
        if let (true, Some(kind), Some(conversation)) = (
            transcript.is_file(),
            task.agent_kind,
            task.facts.agent_conversation_id.as_deref(),
        ) {
            let guest = self
                .agents
                .engine(kind)
                .conversation_transcript_path(conversation);
            isolation
                .deliver_file(sandbox_id.to_owned(), transcript, guest)
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        // The arrivals are the only copy of what came off the other machine, so they
        // are dropped once they are in the sandbox and not before.
        let _ = fs::remove_dir_all(&arrivals);
        Ok(())
    }

    pub async fn finish(
        &self,
        task: &Task,
        agent_run: &TaskAgentRun,
        event: CommandEvent,
        diagnostic: Option<&str>,
    ) -> Result<Task, TaskServiceError> {
        let (exit_code, error) = match event {
            // A clean exit reached here is not an ending the task has words for: a
            // turn that exits without raising anything is resumed by the loop, so this
            // is only the command that ended before the loop could read it.
            CommandEvent::Exited(0) => (
                Some(0),
                "the run ended without saying what happened".to_owned(),
            ),
            CommandEvent::Exited(code) => (Some(code), format!("command exited with code {code}")),
            CommandEvent::Failed(error) => (None, error),
            CommandEvent::Output { .. } => unreachable!("output is handled by the event_loop"),
        };
        let error = diagnostic.map_or(error.clone(), |detail| format!("{error}: {detail}"));
        let mut events = vec![run_ended(agent_run, TaskAgentRunOutcome::Failed, exit_code)];
        events.extend(self.stopped_events(error));
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
    pub(crate) async fn release_resources(&self, task: &Task) -> Result<Task, TaskServiceError> {
        self.release_agent_channel(&task.id);
        self.release_ports(task);
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
    // A limit that clears puts a task back without anyone being committed to it: the
    // same shape as a restart resuming what it was running, on a shorter clock. The
    // task is Pending again, which is what the queue already knows how to start. A
    // hold with no clock on it is not a hold, so it comes back rather than staying
    // stopped with nothing to wake it.
    pub async fn resume_limits_due(&self, tasks: &[Task]) -> Vec<Result<Task, TaskServiceError>> {
        let due = tasks.iter().filter(|task| {
            task.facts.execution_status == TaskExecutionStatus::Paused
                && task.facts.execution_pause_reason
                    == Some(TaskExecutionPauseReason::AwaitingLimit)
                && task
                    .facts
                    .resumes_at_ms
                    .is_none_or(|clears_at_ms| clears_at_ms <= now_ms())
        });
        join_all(due.map(|task| {
            let service = self.clone();
            async move {
                let event = resumed(task, task.facts.snapshot_path.clone().unwrap_or_default());
                service.record(&task.id, &[event]).await
            }
        }))
        .await
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
                            let event = resumed(&task, path.to_string_lossy().into_owned());
                            task = service.record_snapshot(&task, &[event]).await?;
                        }
                        Err(error) if error.is_missing_sandbox() => {
                            // The machine went away under this task and its capsule
                            // with it: there is nothing to snapshot and nothing to
                            // resume from. Leaving it Running is how a task ends up
                            // claiming to work with nothing running behind it, so it
                            // is queued instead, and told what happened to it.
                            tracing::info!(
                                task_id = %task.id,
                                "a task's sandbox did not survive the restart"
                            );
                            let mut events = vec![TaskHistoryEvent::ExecutionQueued {
                                execution_status: TaskExecutionStatus::Pending,
                            }];
                            events.extend(restart_events(
                                &task,
                                "This task's sandbox did not survive the restart, so it starts \
                                 again from the project's repository.",
                            ));
                            task = service.record(&task.id, &events).await?;
                        }
                        Err(error) => {
                            service
                                .fail(
                                    &task,
                                    format!(
                                        "could not recover the task sandbox from {}: {error}",
                                        service.snapshots_dir.display()
                                    ),
                                )
                                .await?;
                            return Err(TaskServiceError::Isolation(error));
                        }
                    }
                } else if task.facts.snapshot_available
                    && task.facts.execution_pause_reason
                        == Some(TaskExecutionPauseReason::ApplicationShutdown)
                {
                    // The shutdown had already suspended the capsule, so there is
                    // nothing to stop and no live sandbox to stop it: the task was
                    // left holding a snapshot and nothing picked it up again. What the
                    // machine does now depends on what it cut in half: a task that was
                    // waiting is still waiting, and a task that was working has a turn
                    // to finish.
                    // A history that cannot be read is not a history that says the task
                    // was idle: saying nothing happened and saying nothing are the same
                    // answer here, and only one of them is honest.
                    let interrupted = match service.events(&task.id).await {
                        Ok(history) => {
                            let events: Vec<_> = history
                                .into_iter()
                                .map(|(_, event)| event)
                                .collect();
                            was_running(&events)
                        }
                        Err(error) => {
                            tracing::warn!(
                                task = %task.id,
                                %error,
                                "could not read a task's history to see what the restart cut in half"
                            );
                            false
                        }
                    };
                    let mut events = vec![if interrupted {
                        TaskHistoryEvent::ExecutionRecovered {
                            execution_status: TaskExecutionStatus::Pending,
                            sandbox_available: false,
                            snapshot_available: true,
                            snapshot_path: task.facts.snapshot_path.clone().unwrap_or_default(),
                        }
                    } else {
                        resumed(&task, task.facts.snapshot_path.clone().unwrap_or_default())
                    }];
                    events.extend(restart_events(&task, &woke_up_saying(&task, interrupted)));
                    task = service.record(&task.id, &events).await?;
                } else if task.facts.deferred_review.is_some() && task.facts.snapshot_available {
                    // A shutdown can catch a review between the hold being
                    // recorded and its capsule being suspended, which leaves it
                    // paused for the shutdown — a pause recovery resumes.
                    let mut events = vec![TaskHistoryEvent::ExecutionPaused {
                        execution_status: TaskExecutionStatus::Paused,
                        sandbox_available: false,
                        snapshot_available: true,
                        execution_pause_reason: TaskExecutionPauseReason::AwaitingReviewChecks,
                        snapshot_path: task.facts.snapshot_path.clone().unwrap_or_default(),
                    }];
                    events.extend(restart_events(&task, &woke_up_saying(&task, false)));
                    task = service.record(&task.id, &events).await?;
                }
                Ok(task)
            }
        }))
        .await
    }
    pub async fn events(
        &self,
        task_id: &str,
    ) -> Result<Vec<(i64, TaskHistoryEvent)>, TaskServiceError> {
        self.store
            .events(task_id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn suspend_all(
        &self,
        tasks: Vec<Task>,
        reason: TaskExecutionPauseReason,
    ) -> Vec<Result<(), TaskServiceError>> {
        join_all(tasks.into_iter().filter(should_suspend).map(|task| {
            let service = self.clone();
            let reason = reason.clone();
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
                                execution_pause_reason: reason,
                                snapshot_path,
                            }
                        };
                        service.record_snapshot(&task, &[event]).await?;
                        service.release_agent_channel(&task.id);
                        Ok(())
                    }
                    Err(error) if error.is_missing_sandbox() => {
                        // There is no capsule to stop, so there is nothing to keep:
                        // the pause is recorded without one, and the next run builds
                        // it again.
                        service
                            .record(
                                &task.id,
                                &[TaskHistoryEvent::ExecutionPaused {
                                    execution_status: TaskExecutionStatus::Paused,
                                    sandbox_available: false,
                                    snapshot_available: task.facts.snapshot_available,
                                    execution_pause_reason: reason.clone(),
                                    snapshot_path: task
                                        .facts
                                        .snapshot_path
                                        .clone()
                                        .unwrap_or_default(),
                                }],
                            )
                            .await?;
                        Ok(())
                    }
                    // A machine that is going down has nothing to do about it: the
                    // task keeps the state it has, and the boot that follows works out
                    // what survived. Recording a failure here would end a task because
                    // its host restarted, and queuing it would hand work to a process
                    // that is about to be gone.
                    Err(error) if reason == TaskExecutionPauseReason::ApplicationShutdown => {
                        tracing::warn!(
                            task_id = %task.id,
                            %error,
                            "a task could not be saved before the machine stopped"
                        );
                        Ok(())
                    }
                    // Where it was writing is half of why it could not: a permission
                    // denied on one machine is a directory owned by another, and the
                    // operator is the only one who can look.
                    Err(error) => {
                        service
                            .fail(
                                &task,
                                format!(
                                    "could not save the task sandbox to {}: {error}",
                                    service.snapshots_dir.display()
                                ),
                            )
                            .await?;
                        Err(TaskServiceError::Isolation(error))
                    }
                }
            }
        }))
        .await
    }
    pub(crate) async fn record_snapshot(
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
    pub(crate) async fn fail(&self, task: &Task, error: String) -> Result<Task, TaskServiceError> {
        tracing::error!(task_id = %task.id, %error, "task execution failed");
        let events = self.stopped_events(error);
        self.record(&task.id, &events).await
    }

    fn stopped_events(&self, error: String) -> Vec<TaskHistoryEvent> {
        failure_events(error).to_vec()
    }
}

// A restart is the machine's news, not the agent's: it has to know its workspace
// moved under it, and the operator reads it in the record rather than in a question,
// because it asks nothing of anybody.
fn restart_events(task: &Task, said: &str) -> Vec<TaskHistoryEvent> {
    notice_event(
        task,
        &format!(
            "altitude on {} restarted and is picking this task back up. {said}",
            task.host
        ),
    )
}

// The machine telling the agent something it has to know at its next turn: what it is
// starting from, when the workspace it had is gone.
fn notice_event(task: &Task, body: &str) -> Vec<TaskHistoryEvent> {
    vec![TaskHistoryEvent::TaskInputExpressed {
        input: TaskInput {
            id: uuid::Uuid::new_v4().to_string(),
            at_ms: chrono::Utc::now().timestamp_millis(),
            author: TaskInputAuthor::Operator {
                id: task.facts.operator_id.clone().unwrap_or_default(),
                label: task.facts.operator_label.clone().unwrap_or_default(),
            },
            source: TaskInputSource::Notice,
            body: body.to_owned(),
            external_id: None,
        },
    }]
}

// What a restart left this task doing, in the words the operator reads: a task that
// was waiting on a person or on a pull request is not "resuming" anything.
// The last thing the task's history said it was doing: a run that started and never
// ended is a run the machine stopped in the middle of.
fn was_running(events: &[TaskHistoryEvent]) -> bool {
    for event in events.iter().rev() {
        match event {
            TaskHistoryEvent::AgentRunStarted { .. } => return true,
            // The shutdown's own pause is the interrupt being recorded, not the
            // answer to the question: it is written for every task the machine
            // suspends, the ones it cut in half included. Reading it as an ending
            // is how a restart told a task mid-turn that it had been waiting all
            // along, and left the turn unfinished.
            TaskHistoryEvent::ExecutionPaused {
                execution_pause_reason: TaskExecutionPauseReason::ApplicationShutdown,
                ..
            } => {}
            TaskHistoryEvent::AgentRunEnded { .. }
            | TaskHistoryEvent::ExecutionPaused { .. }
            | TaskHistoryEvent::ExecutionRecovered { .. }
            | TaskHistoryEvent::ExecutionQueued { .. }
            | TaskHistoryEvent::TaskCompleted { .. }
            | TaskHistoryEvent::ExecutionFailed { .. } => return false,
            _ => {}
        }
    }
    false
}

fn woke_up_saying(task: &Task, interrupted: bool) -> String {
    if interrupted {
        return "Its run was cut in half by the restart, so it picks the work back up.".to_owned();
    }
    if task.facts.deferred_review.is_some() {
        return "It was waiting on its pull requests, and the machine is watching them again."
            .to_owned();
    }
    if task.facts.waits_on_operator() {
        return "It was waiting on the operator, and still is.".to_owned();
    }
    "Its sandbox was saved when the machine went down, so the work resumes where it left off."
        .to_owned()
}

fn resumed(task: &Task, snapshot_path: String) -> TaskHistoryEvent {
    if task.facts.deferred_review.is_some() {
        TaskHistoryEvent::ExecutionPaused {
            execution_status: TaskExecutionStatus::Paused,
            sandbox_available: false,
            snapshot_available: true,
            execution_pause_reason: TaskExecutionPauseReason::AwaitingReviewChecks,
            snapshot_path,
        }
    } else if task.facts.waits_on_operator() {
        TaskHistoryEvent::ExecutionPaused {
            execution_status: TaskExecutionStatus::Paused,
            sandbox_available: false,
            snapshot_available: true,
            execution_pause_reason: TaskExecutionPauseReason::AwaitingOperator,
            snapshot_path,
        }
    } else {
        TaskHistoryEvent::ExecutionRecovered {
            execution_status: TaskExecutionStatus::Pending,
            sandbox_available: false,
            snapshot_available: true,
            snapshot_path,
        }
    }
}
