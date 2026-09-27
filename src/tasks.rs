use crate::{
    agent::{AgentEngine, AgentError, AgentKind, ClaudeCode},
    isolation::{CommandEvent, CommandSession, IsolationEngine, IsolationError, SandboxOptions},
    vcs::GitHub,
};
use futures_util::future::join_all;
use std::{error::Error, fmt, path::PathBuf};
use uuid::Uuid;
mod state;
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

    pub async fn create_claude(&self, intent: String) -> Result<Task, TaskServiceError> {
        self.store
            .create_with_agent(
                intent,
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
    ) -> Result<(Task, Vec<TaskOperatorNotice>), TaskServiceError> {
        let attention = report
            .operator_attention_level
            .as_ref()
            .unwrap_or(&task.current_state.operator_attention_level);
        if (*attention == TaskOperatorAttentionLevel::Requested
            && report.requested_action != TaskAgentRequestedAction::AwaitOperator)
            || (report.requested_action == TaskAgentRequestedAction::AwaitOperator
                && *attention == TaskOperatorAttentionLevel::Nominal)
        {
            return Err(TaskServiceError::InvalidReport("requested attention requires await_operator; await_operator requires requested attention or alarm".into()));
        }
        self.store
            .record_report(&task.id, agent_run, report)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn unacknowledged_operator_notices(
        &self,
        level: TaskOperatorNoticeLevel,
    ) -> Result<Vec<TaskOperatorNotice>, TaskServiceError> {
        self.store
            .unacknowledged_operator_notices(level)
            .await
            .map_err(TaskServiceError::Task)
    }

    pub async fn acknowledge_operator_notice(&self, id: &str) -> Result<(), TaskServiceError> {
        self.store
            .acknowledge_operator_notice(id)
            .await
            .map_err(TaskServiceError::Task)
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
        let mut task = self.provision(task).await?;
        task.current_state.execution_controller = TaskExecutionController::Altitude;
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
                    &task.autopilot_context(),
                    resume,
                )
            }
            None => task.current_state.intent.clone(),
        };
        self.launch(task, command).await
    }

    async fn provision(&self, task: Task) -> Result<Task, TaskServiceError> {
        let sandbox_id = format!("altitude-{}", Uuid::new_v4());
        let create = match task.agent_kind {
            Some(AgentKind::Claude) => {
                self.isolation
                    .provision(
                        sandbox_id.clone(),
                        task.current_state.snapshot_path.clone(),
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

    async fn start_autopilot_turn(
        &self,
        mut task: Task,
        prompt: &str,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        let conversation_id = self.claude_conversation(&task, true).await?;
        task.current_state.execution_controller = TaskExecutionController::Altitude;
        let command = self.claude.autopilot_command(
            &conversation_id,
            prompt,
            &task.autopilot_context(),
            true,
        );
        self.launch(task, command).await
    }

    pub async fn request_autopilot_report(
        &self,
        task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        let conversation_id = self.claude_conversation(&task, true).await?;
        let command = self
            .claude
            .report_command(&conversation_id, &task.autopilot_context());
        self.launch(task, command).await
    }

    pub async fn start_operator(
        &self,
        mut task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        if task.current_state.sandbox_id.is_none() {
            task = self.provision(task).await?;
        }
        let conversation_id = self.claude_conversation(&task, true).await?;
        task.current_state.execution_controller = TaskExecutionController::Operator;
        self.launch(task, self.claude.operator_command(&conversation_id))
            .await
    }

    async fn launch(
        &self,
        task: Task,
        command: String,
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
                    controller: task.current_state.execution_controller.clone(),
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
            CommandEvent::Output { .. } => unreachable!("output is handled by the runtime"),
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
