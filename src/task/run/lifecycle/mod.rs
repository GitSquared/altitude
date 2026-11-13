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

mod events;
mod release;

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
                // Whatever woke this task says what the turn opens with. Anything else
                // is a paused capsule coming back, whose last turn may be days old, so
                // the agent is told what happened to it rather than asked to resume.
                let prompt = task.facts.wake_news.clone().unwrap_or_else(|| {
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
}

// The machine telling the agent something it has to know at its next turn: what it is
// starting from, when the workspace it had is gone.
pub(super) fn notice_event(task: &Task, body: &str) -> Vec<TaskHistoryEvent> {
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
