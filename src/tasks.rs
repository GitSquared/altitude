use crate::development::DevelopmentEnvironment;
use crate::{
    agent::{AgentEngine, AgentError, AgentKind, ClaudeCode},
    isolation::{
        CommandEvent, CommandSession, IsolationEngine, IsolationError, OutputStream, SandboxOptions,
    },
    vcs::{GitHub, VcsProvider},
};
use futures_util::future::join_all;
use std::{error::Error, fmt, path::PathBuf};
use uuid::Uuid;
mod event_loop;
mod state;
pub use event_loop::{TaskEventLoop, TaskExecutionUpdate};
mod store;
pub use state::*;
pub use store::TaskStore;

#[derive(Clone)]
pub struct TaskService<E> {
    store: TaskStore,
    isolation: E,
    snapshots_dir: PathBuf,
    claude: ClaudeCode,
}

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    pub fn new(store: TaskStore, isolation: E, snapshots_dir: PathBuf, claude: ClaudeCode) -> Self {
        Self {
            store,
            isolation,
            snapshots_dir,
            claude,
        }
    }

    pub async fn list(&self) -> Result<Vec<Task>, TaskServiceError> {
        self.store.list().await.map_err(TaskServiceError::Task)
    }

    pub async fn create_claude(
        &self,
        intent: String,
        environment: DevelopmentEnvironment,
    ) -> Result<Task, TaskServiceError> {
        self.store
            .create_with_agent(
                intent,
                environment,
                Some(AgentKind::Claude),
                Some(self.claude.new_conversation_id()),
            )
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn apply_report(
        &self,
        task: &Task,
        agent_run: &TaskAgentRun,
        report: &TaskAgentTurnReport,
    ) -> Result<Task, TaskServiceError> {
        let attention = match (&report.requested_action, &report.operator_signal) {
            (TaskAgentRequestedAction::Continue, None)
            | (
                TaskAgentRequestedAction::Continue,
                Some(TaskOperatorSignalDraft {
                    kind: TaskOperatorSignalKind::Update,
                    ..
                }),
            )
            | (TaskAgentRequestedAction::Complete, None) => TaskOperatorAttentionLevel::Nominal,
            (
                TaskAgentRequestedAction::AwaitOperator,
                Some(TaskOperatorSignalDraft {
                    kind: TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::Review,
                    ..
                }),
            ) => TaskOperatorAttentionLevel::Requested,
            _ => {
                return Err(TaskServiceError::InvalidReport(
                    "updates must continue; decisions and reviews must await the operator; completion cannot raise a signal".into(),
                ));
            }
        };
        let github = GitHub;
        let contributions: Vec<_> = report
            .code_contribution_urls
            .iter()
            .filter_map(|url| {
                let contribution = github.contribution_from_url(url);
                if contribution.is_none() {
                    tracing::warn!(task_id = %task.id, %url, "ignoring unsupported code contribution URL");
                }
                contribution
            })
            .collect();
        self.store
            .record_report(&task.id, agent_run, report, attention, &contributions)
            .await
            .map_err(TaskServiceError::Task)
    }

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
        if task.current_state.sandbox_id.is_none() {
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

    async fn record(
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
        let resume = task.current_state.snapshot_path.is_some();
        let task = self.provision(task).await?;
        let command = match task.agent_kind {
            Some(AgentKind::Claude) => {
                let conversation_id = self.claude_conversation(&task, resume).await?;
                self.claude.autopilot_command(
                    &conversation_id,
                    if resume {
                        "Resume the task and continue making progress."
                    } else {
                        &task.current_state.intent
                    },
                    &task.turn_context(TaskExecutionMode::Autopilot),
                    resume,
                )
            }
            None => task.current_state.intent.clone(),
        };
        self.launch(task, command, TaskExecutionMode::Autopilot)
            .await
    }

    async fn provision(&self, task: Task) -> Result<Task, TaskServiceError> {
        let sandbox_id = format!("altitude-{}", Uuid::new_v4());
        let create = match task.agent_kind {
            Some(AgentKind::Claude) => {
                self.isolation
                    .provision(
                        sandbox_id.clone(),
                        task.current_state.snapshot_path.clone(),
                        task.development_environment(),
                        vec![
                            self.claude.environment_recipe(),
                            GitHub::environment_recipe(),
                        ],
                    )
                    .await
            }
            None => {
                self.isolation
                    .create(
                        sandbox_id.clone(),
                        SandboxOptions::isolated(task.current_state.snapshot_path.clone()),
                    )
                    .await
            }
        };
        if let Err(error) = create {
            self.fail(&task, error.to_string()).await?;
            return Err(TaskServiceError::Isolation(error));
        }
        match self
            .record(
                &task.id,
                &[TaskHistoryEvent::SandboxProvisioned {
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

    pub async fn continue_autopilot(
        &self,
        task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        self.start_autopilot_turn(
            task,
            "Continue the task autonomously from the current state.",
        )
        .await
    }

    pub async fn resume_autopilot_after_operator(
        &self,
        task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        self.start_autopilot_turn(task, "The operator has returned control to Altitude. Review the preceding conversation and current workspace state, then continue the task autonomously wherever the original intent reasonably permits. Do not wait for routine confirmation.").await
    }

    /// Run a command in the task's capsule and return its stdout.
    ///
    /// Only stdout is collected: providers emit structured output there and may
    /// write progress to stderr, which would otherwise corrupt the parse.
    async fn run_captured(
        &self,
        sandbox_id: &str,
        command: String,
    ) -> Result<String, TaskServiceError> {
        let mut session = self
            .isolation
            .run_command(sandbox_id.to_owned(), command)
            .await
            .map_err(TaskServiceError::Isolation)?;
        let mut output = String::new();
        while let Some(event) = session.events.recv().await {
            match event {
                CommandEvent::Output {
                    stream: OutputStream::Stdout,
                    bytes,
                } => output.push_str(&String::from_utf8_lossy(&bytes)),
                CommandEvent::Output { .. } => {}
                CommandEvent::Exited(0) => return Ok(output),
                CommandEvent::Exited(code) => {
                    return Err(TaskServiceError::Isolation(IsolationError::new(format!(
                        "observation command exited with code {code}"
                    ))));
                }
                CommandEvent::Failed(error) => {
                    return Err(TaskServiceError::Isolation(IsolationError::new(error)));
                }
            }
        }
        Err(TaskServiceError::Isolation(IsolationError::new(
            "observation command closed without an exit status",
        )))
    }

    /// Refresh the external state of every contribution linked to a task.
    ///
    /// Runs inside the task's own capsule, where the provider CLI and its
    /// credentials already live, so watching a contribution asks the operator
    /// for no configuration anywhere. Best-effort by design: a provider that is
    /// unreachable or unauthenticated must not stop the task, so failures are
    /// logged and the turn proceeds with the state already recorded.
    async fn observe_code_contributions(&self, task: Task) -> Task {
        let Some(sandbox_id) = task.current_state.sandbox_id.clone() else {
            return task;
        };
        let github = GitHub;
        let mut observations = Vec::new();
        for contribution in &task.code_contributions {
            let command = github.observation_command(contribution);
            let output = match self.run_captured(&sandbox_id, command).await {
                Ok(output) => output,
                Err(error) => {
                    tracing::warn!(task_id = %task.id, %error, "could not observe code contribution");
                    continue;
                }
            };
            match github.contribution_observation(contribution, &output) {
                Some(observation) => observations.push(observation),
                None => tracing::warn!(
                    task_id = %task.id,
                    contribution = %contribution.id,
                    "could not read code contribution state"
                ),
            }
        }
        if observations.is_empty() {
            return task;
        }
        match self
            .store
            .record_code_contribution_observations(&task.id, &observations)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                tracing::warn!(%error, "could not record code contribution observations");
                task
            }
        }
    }

    async fn start_autopilot_turn(
        &self,
        task: Task,
        prompt: &str,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        // Refresh what the contributions look like before the agent runs, so a
        // turn is planned against current external state rather than the state
        // as of the previous turn.
        let task = self.observe_code_contributions(task).await;
        let conversation_id = self.claude_conversation(&task, true).await?;
        let command = self.claude.autopilot_command(
            &conversation_id,
            prompt,
            &task.turn_context(TaskExecutionMode::Autopilot),
            true,
        );
        self.launch(task, command, TaskExecutionMode::Autopilot)
            .await
    }

    pub async fn request_autopilot_report(
        &self,
        task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        let conversation_id = self.claude_conversation(&task, true).await?;
        let command = self.claude.report_command(
            &conversation_id,
            &task.turn_context(TaskExecutionMode::AutopilotReport),
        );
        self.launch(task, command, TaskExecutionMode::AutopilotReport)
            .await
    }

    pub async fn start_operator(
        &self,
        mut task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        if task.current_state.sandbox_id.is_none() {
            task = self.provision(task).await?;
        }
        let conversation_id = self.claude_conversation(&task, true).await?;
        self.launch(
            task,
            self.claude.operator_command(&conversation_id),
            TaskExecutionMode::Operator,
        )
        .await
    }

    async fn launch(
        &self,
        task: Task,
        command: String,
        mode: TaskExecutionMode,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        let sandbox_id = task
            .current_state
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let agent_run = TaskAgentRun {
            id: Uuid::new_v4().to_string(),
            task_id: task.id.clone(),
        };
        let task = self
            .record(
                &task.id,
                &[TaskHistoryEvent::AgentRunStarted {
                    agent_run_id: agent_run.id.clone(),
                    sandbox_id: sandbox_id.clone(),
                    mode,
                }],
            )
            .await?;
        match self.isolation.run_command(sandbox_id, command).await {
            Ok(session) => Ok((task, agent_run, session)),
            Err(error) => {
                self.finish(&task, &agent_run, CommandEvent::Failed(error.to_string()))
                    .await?;
                Err(TaskServiceError::Isolation(error))
            }
        }
    }

    pub async fn await_operator(
        &self,
        task: &Task,
        agent_run: &TaskAgentRun,
    ) -> Result<Task, TaskServiceError> {
        let sandbox_id = task
            .current_state
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let snapshot_path = match self
            .isolation
            .suspend(
                sandbox_id,
                snapshot_name(&task.id),
                self.snapshots_dir.clone(),
            )
            .await
        {
            Ok(path) => path,
            Err(error) => {
                self.fail(task, error.to_string()).await?;
                return Err(TaskServiceError::Isolation(error));
            }
        };
        self.record_snapshot(
            task,
            &[
                run_ended(agent_run, TaskAgentRunOutcome::AwaitingOperator, Some(0)),
                TaskHistoryEvent::ExecutionPaused {
                    reason: TaskExecutionPauseReason::AwaitingOperator,
                    snapshot_path: snapshot_path.to_string_lossy().into_owned(),
                },
            ],
        )
        .await
    }

    pub async fn continue_turn(&self, agent_run: &TaskAgentRun) -> Result<(), TaskServiceError> {
        self.record(
            &agent_run.task_id,
            &[run_ended(
                agent_run,
                TaskAgentRunOutcome::Continued,
                Some(0),
            )],
        )
        .await?;
        Ok(())
    }

    pub async fn complete(
        &self,
        task: &Task,
        agent_run: &TaskAgentRun,
    ) -> Result<Task, TaskServiceError> {
        if let Some(sandbox_id) = &task.current_state.sandbox_id
            && let Err(error) = self.isolation.remove(sandbox_id.clone()).await
        {
            self.fail(task, error.to_string()).await?;
            return Err(TaskServiceError::Isolation(error));
        }
        if let Some(snapshot_path) = &task.current_state.snapshot_path
            && let Err(error) = self
                .isolation
                .remove_snapshot(PathBuf::from(snapshot_path))
                .await
        {
            self.fail(task, error.to_string()).await?;
            return Err(TaskServiceError::Isolation(error));
        }
        self.record(
            &task.id,
            &[
                run_ended(agent_run, TaskAgentRunOutcome::Completed, Some(0)),
                TaskHistoryEvent::TaskCompleted,
            ],
        )
        .await
    }

    pub async fn finish(
        &self,
        task: &Task,
        agent_run: &TaskAgentRun,
        event: CommandEvent,
    ) -> Result<Task, TaskServiceError> {
        let (exit_code, error) = match event {
            CommandEvent::Exited(0) => return self.complete(task, agent_run).await,
            CommandEvent::Exited(code) => (Some(code), format!("command exited with code {code}")),
            CommandEvent::Failed(error) => (None, error),
            CommandEvent::Output { .. } => unreachable!("output is handled by the event_loop"),
        };
        self.record(
            &task.id,
            &[
                run_ended(agent_run, TaskAgentRunOutcome::Failed, exit_code),
                TaskHistoryEvent::ExecutionFailed { error },
            ],
        )
        .await
    }

    pub async fn remove(&self, task: Task) -> Result<(), TaskServiceError> {
        if let Some(sandbox_id) = task.current_state.sandbox_id {
            self.isolation
                .remove(sandbox_id)
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        if let Some(snapshot_path) = task.current_state.snapshot_path {
            self.isolation
                .remove_snapshot(PathBuf::from(snapshot_path))
                .await
                .map_err(TaskServiceError::Isolation)?;
        }
        self.store
            .delete(&task.id)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn resume_all(
        &self,
    ) -> Vec<Result<(Task, TaskAgentRun, CommandSession), TaskServiceError>> {
        let tasks = match self.list().await {
            Ok(tasks) => tasks,
            Err(error) => return vec![Err(error)],
        };
        join_all(tasks.into_iter().filter(Task::should_resume).map(|task| {
            let service = self.clone();
            async move {
                let mut task = task;
                if let Some(sandbox_id) = &task.current_state.sandbox_id {
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
                service.start(task).await
            }
        }))
        .await
    }

    pub async fn suspend_all(&self, tasks: Vec<Task>) -> Vec<Result<(), TaskServiceError>> {
        join_all(tasks.into_iter().filter(Task::should_suspend).map(|task| {
            let service = self.clone();
            async move {
                let sandbox_id = task
                    .current_state
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
                        let event =
                            if task.current_state.execution_status == TaskExecutionStatus::Failed {
                                TaskHistoryEvent::TaskSnapshotSaved { snapshot_path }
                            } else {
                                TaskHistoryEvent::ExecutionPaused {
                                    reason: TaskExecutionPauseReason::ApplicationShutdown,
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

    async fn record_snapshot(
        &self,
        task: &Task,
        events: &[TaskHistoryEvent],
    ) -> Result<Task, TaskServiceError> {
        let updated = self.record(&task.id, events).await?;
        if let Some(previous) = &task.current_state.snapshot_path
            && updated.current_state.snapshot_path.as_ref() != Some(previous)
            && let Err(error) = self
                .isolation
                .remove_snapshot(PathBuf::from(previous))
                .await
        {
            tracing::warn!(task_id = %task.id, %error, "could not remove superseded task snapshot");
        }
        Ok(updated)
    }

    async fn fail(&self, task: &Task, error: String) -> Result<Task, TaskServiceError> {
        tracing::error!(task_id = %task.id, %error, "task execution failed");
        self.record(&task.id, &[TaskHistoryEvent::ExecutionFailed { error }])
            .await
    }

    async fn claude_conversation(
        &self,
        task: &Task,
        require_transcript: bool,
    ) -> Result<String, TaskServiceError> {
        let Some(conversation_id) = task.current_state.agent_conversation_id.as_deref() else {
            return self
                .conversation_failure(task, "Claude task has no conversation ID")
                .await;
        };
        if !require_transcript {
            return Ok(conversation_id.to_owned());
        }
        let transcript_path = self.claude.conversation_transcript_path(conversation_id);
        let sandbox_id = task
            .current_state
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        match self.isolation.guest_regular_file_exists(sandbox_id, transcript_path).await {
            Ok(true) => Ok(conversation_id.to_owned()),
            Ok(false) => self.conversation_failure(task, "Claude conversation transcript is missing; the task cannot resume without its session state").await,
            Err(error) => self.conversation_failure(task, format!("could not read Claude conversation transcript: {error}")).await,
        }
    }

    async fn conversation_failure<T>(
        &self,
        task: &Task,
        message: impl Into<String>,
    ) -> Result<T, TaskServiceError> {
        let error = AgentError::new(message);
        self.fail(task, error.to_string()).await?;
        Err(TaskServiceError::Agent(error))
    }
}

fn snapshot_name(task_id: &str) -> String {
    format!("task-{task_id}-{}", Uuid::new_v4())
}

fn run_ended(
    agent_run: &TaskAgentRun,
    outcome: TaskAgentRunOutcome,
    exit_code: Option<i32>,
) -> TaskHistoryEvent {
    TaskHistoryEvent::AgentRunEnded {
        agent_run_id: agent_run.id.clone(),
        outcome,
        exit_code,
    }
}

#[derive(Debug)]
pub enum TaskError {
    Database(sqlx::Error),
    HistoryData(serde_json::Error),
    MissingSandbox,
}
impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "task database error: {error}"),
            Self::HistoryData(error) => write!(formatter, "invalid task history data: {error}"),
            Self::MissingSandbox => formatter.write_str("task has no running sandbox"),
        }
    }
}
impl Error for TaskError {}

#[derive(Debug)]
pub enum TaskServiceError {
    Agent(AgentError),
    Task(TaskError),
    Isolation(IsolationError),
    InvalidReport(String),
}
impl fmt::Display for TaskServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => error.fmt(formatter),
            Self::Task(error) => error.fmt(formatter),
            Self::Isolation(error) => error.fmt(formatter),
            Self::InvalidReport(error) => write!(formatter, "invalid agent turn report: {error}"),
        }
    }
}
impl Error for TaskServiceError {}
