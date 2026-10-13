//! Driving a task: one turn at a time, by autopilot or by an operator.

use crate::{
    isolation::{CommandEvent, CommandSession, IsolationEngine, OutputStream},
    vcs::{GitHub, VcsProvider},
};
use uuid::Uuid;

use super::{
    IsolationError, Task, TaskAgentRequestedAction, TaskAgentRun, TaskAgentRunOutcome,
    TaskAgentTurnReport, TaskError, TaskExecutionMode, TaskExecutionPauseReason,
    TaskExecutionStatus, TaskHistoryEvent, TaskOperatorAttentionLevel, TaskOperatorSignalDraft,
    TaskOperatorSignalKind, TaskService, TaskServiceError, run_ended, snapshot_name,
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone,
{
    /// Record that the operator asked for the task to be driven differently.
    ///
    /// The request is addressed to the run it was asked during, so a later run
    /// cannot pick it up as though it were still current.
    pub async fn request_control(
        &self,
        task_id: &str,
        requested_mode: TaskExecutionMode,
        agent_run_id: &str,
    ) -> Result<Task, TaskServiceError> {
        self.store
            .request_control(task_id, requested_mode, agent_run_id)
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
            ) => TaskOperatorAttentionLevel::Nominal,
            (
                TaskAgentRequestedAction::AwaitOperator,
                Some(TaskOperatorSignalDraft {
                    kind: TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::InReview,
                    ..
                }),
            ) => TaskOperatorAttentionLevel::Requested,
            _ => {
                return Err(TaskServiceError::InvalidReport(
                    "updates must continue; decisions and wrap-up requests must await the operator"
                        .into(),
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
    pub(super) async fn run_captured(
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
    pub(super) async fn observe_code_contributions(&self, task: Task) -> Task {
        if !task.facts.sandbox_available {
            return task;
        }
        let Some(sandbox_id) = task.facts.sandbox_id.clone() else {
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
    pub(super) async fn start_autopilot_turn(
        &self,
        task: Task,
        prompt: &str,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        // Refresh what the contributions look like before the agent runs, so a
        // turn is planned against current external state rather than the state
        // as of the previous turn.
        let task = self.observe_code_contributions(task).await;
        let engine = self.engine(&task)?;
        let conversation_id = self.agent_conversation(&task, true).await?;
        let command = engine.autopilot_command(
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
        let engine = self.engine(&task)?;
        let conversation_id = self.agent_conversation(&task, true).await?;
        let command = engine.report_command(
            &conversation_id,
            &task.turn_context(TaskExecutionMode::AutopilotReport),
        );
        self.launch(task, command, TaskExecutionMode::AutopilotReport)
            .await
    }
    /// Hand the task's terminal to the operator.
    ///
    /// The conversation check is waived for harnesses that create a session on
    /// demand, so an operator can attach and authenticate before a task's first
    /// turn has ever succeeded.
    pub async fn start_operator(
        &self,
        mut task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        if task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::Transfer) {
            return Err(TaskServiceError::Task(TaskError::OperationRejected(
                "the task is awaiting transfer confirmation",
            )));
        }
        if !task.facts.sandbox_available {
            task = self.provision(task).await?;
        }
        let engine = self.engine(&task)?;
        let conversation_id = self
            .agent_conversation(&task, engine.operator_requires_conversation())
            .await?;
        self.launch(
            task,
            engine.operator_command(&conversation_id),
            TaskExecutionMode::Operator,
        )
        .await
    }
    pub(super) async fn launch(
        &self,
        task: Task,
        command: String,
        mode: TaskExecutionMode,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        let sandbox_id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let agent_run = TaskAgentRun {
            id: Uuid::new_v4().to_string(),
            task_id: task.id.clone(),
        };
        let mut events = Vec::new();
        if mode == TaskExecutionMode::Autopilot {
            // Returning from an operator shell is also an explicit retry. Retire
            // its old failure signals; another failed attempt raises fresh evidence.
            events.extend(
                self.store
                    .operator_signals(&task.id)
                    .await
                    .map_err(TaskServiceError::Task)?
                    .into_iter()
                    .filter(|signal| signal.kind == TaskOperatorSignalKind::Failure)
                    .map(|signal| TaskHistoryEvent::OperatorSignalAcknowledged {
                        signal_id: signal.id,
                    }),
            );
        }
        events.push(TaskHistoryEvent::AgentRunStarted {
            execution_status: TaskExecutionStatus::Running,
            agent_run_id: agent_run.id.clone(),
            sandbox_id: sandbox_id.clone(),
            execution_mode: mode,
        });
        let task = self.record(&task.id, &events).await?;
        match self.isolation.run_command(sandbox_id, command).await {
            Ok(session) => Ok((task, agent_run, session)),
            Err(error) => {
                self.finish(
                    &task,
                    &agent_run,
                    CommandEvent::Failed(error.to_string()),
                    None,
                )
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
            .facts
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
        let task = self
            .record_snapshot(
                task,
                &[
                    run_ended(agent_run, TaskAgentRunOutcome::AwaitingOperator, Some(0)),
                    TaskHistoryEvent::ExecutionPaused {
                        execution_status: TaskExecutionStatus::Paused,
                        sandbox_available: false,
                        snapshot_available: true,
                        execution_pause_reason: TaskExecutionPauseReason::AwaitingOperator,
                        snapshot_path: snapshot_path.to_string_lossy().into_owned(),
                    },
                ],
            )
            .await?;
        // The sandbox is gone, so nothing can reach the channel until the task
        // is provisioned again.
        self.release_agent_channel(&task.id);
        Ok(task)
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
}
