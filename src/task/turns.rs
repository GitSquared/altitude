//! Driving a task: one turn at a time, by autopilot or by an operator.

use crate::{
    isolation::{
        CommandEvent, CommandSession, IsolationEngine, OutputStream, Unwatched, with_helper,
    },
    project::Project,
    vcs::{self, ContributionObservation, ProviderBatch},
};
use uuid::Uuid;

use super::{
    IsolationError, Task, TaskAgentRequestedAction, TaskAgentRun, TaskAgentRunOutcome,
    TaskAgentTurnReport, TaskError, TaskExecutionMode, TaskExecutionPauseReason,
    TaskExecutionStatus, TaskHistoryEvent, TaskOperatorAttentionLevel, TaskOperatorSignalAuthor,
    TaskOperatorSignalDraft, TaskOperatorSignalKind, TaskService, TaskServiceError, run_ended,
    snapshot_name, store::ReviewRequest,
};

/// What the host says when a wrap-up request arrives with no pull request linked.
const NO_PULL_REQUEST_LINKED: &str = "No pull request is linked to this task, so Altitude watched none. If there is one, send the task back and ask the agent to link it.";

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
    ) -> Result<(Task, TaskAgentRequestedAction), TaskServiceError> {
        let attention = match (&report.requested_action, &report.operator_signal) {
            (TaskAgentRequestedAction::Continue, None)
            | (
                TaskAgentRequestedAction::Continue,
                Some(TaskOperatorSignalDraft {
                    kind: TaskOperatorSignalKind::Update | TaskOperatorSignalKind::Decision,
                    ..
                }),
            ) => TaskOperatorAttentionLevel::Nominal,
            (
                TaskAgentRequestedAction::AwaitOperator,
                Some(TaskOperatorSignalDraft {
                    kind: TaskOperatorSignalKind::Blocked | TaskOperatorSignalKind::InReview,
                    ..
                }),
            ) => TaskOperatorAttentionLevel::Requested,
            _ => {
                return Err(TaskServiceError::InvalidReport(
                    "updates and decisions continue; blocked questions and review requests await the operator"
                        .into(),
                ));
            }
        };
        // Links are recorded by the vcs_link_pull_request tool during the work
        // turn, so the task this run started from may predate them. Read the
        // current links before deciding whether a wrap-up request can be held.
        let current = self
            .store
            .get(&task.id)
            .await
            .map_err(TaskServiceError::Task)?;
        let task = current.as_ref().unwrap_or(task);
        // A wrap-up request with linked pull requests is held, not delivered: the
        // host watches those contributions and decides whether the operator or the
        // agent is next. The links are already in the store, so a restart resumes
        // watching every pull request the task linked.
        //
        // One with no pull request linked goes out at once, saying so: the host
        // watches linked pull requests, and an agent that forgot to link one must
        // not read as a task that has none.
        let review = match report.operator_signal.as_ref() {
            Some(signal) if signal.kind == TaskOperatorSignalKind::InReview => {
                if task.code_contributions.is_empty() {
                    ReviewRequest::RaiseWithNote(NO_PULL_REQUEST_LINKED)
                } else {
                    ReviewRequest::Hold(signal)
                }
            }
            _ => ReviewRequest::Raise,
        };
        let task = self
            .store
            .record_report(&task.id, agent_run, report, attention, review)
            .await
            .map_err(TaskServiceError::Task)?;
        Ok((task, report.requested_action.clone()))
    }
    pub async fn continue_autopilot(
        &self,
        task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        self.start_autopilot_turn(
            task,
            "Continue the task autonomously from the current state.",
            None,
        )
        .await
    }
    pub async fn resume_autopilot_after_operator(
        &self,
        task: Task,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        self.start_autopilot_turn(task, "The operator has returned control to Altitude. Review the preceding conversation and current workspace state, then continue the task autonomously wherever the original intent reasonably permits. Do not wait for routine confirmation.", None).await
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
    async fn contribution_observations(&self, task: &Task) -> Vec<ContributionObservation> {
        if !task.facts.sandbox_available {
            return Vec::new();
        }
        let Some(sandbox_id) = task.facts.sandbox_id.clone() else {
            return Vec::new();
        };
        let mut observations = Vec::new();
        for batch in ProviderBatch::of(&task.code_contributions) {
            let output = match self.run_captured(&sandbox_id, batch.command()).await {
                Ok(output) => output,
                Err(error) => {
                    tracing::warn!(task_id = %task.id, %error, "could not observe code contributions");
                    return Vec::new();
                }
            };
            match batch.observations(&output) {
                Some(read) => observations.extend(read),
                None => {
                    tracing::warn!(task_id = %task.id, "could not read code contribution state");
                    return Vec::new();
                }
            }
        }
        observations
    }
    pub(super) async fn observe_code_contributions(&self, task: Task) -> Task {
        let observations = self.contribution_observations(&task).await;
        if observations.is_empty() {
            return task;
        }
        match self
            .store
            .record_code_contribution_observations(&task.id, &observations)
            .await
        {
            Ok((task, _)) => task,
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
        // The signal thread this turn answers, when the operator started it by
        // replying to one.
        answering: Option<String>,
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
        self.launch(task, command, TaskExecutionMode::Autopilot, answering)
            .await
    }
    pub async fn request_autopilot_report(
        &self,
        task: Task,
        // The thread the run being reported on was answering. The report belongs
        // to it, so the request turn carries it across.
        answering: Option<String>,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        let engine = self.engine(&task)?;
        let conversation_id = self.agent_conversation(&task, true).await?;
        let command = engine.report_command(
            &conversation_id,
            &task.turn_context(TaskExecutionMode::AutopilotReport),
        );
        self.launch(task, command, TaskExecutionMode::AutopilotReport, answering)
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
        // Taking over a held review ends the wait: the operator is looking at the
        // work now, and a hold left behind would keep admission from running the
        // task again after they hand it back.
        if let Some(deferred) = task.facts.deferred_review.clone() {
            // The request was never delivered, so its thread either does not exist
            // or is waiting on the agent. Inspecting the task ends the wait either
            // way, and a later request opens a thread of its own.
            task = self
                .record(
                    &task.id,
                    &[
                        TaskHistoryEvent::ReviewDeferralCleared {
                            signal_id: deferred.signal.id.clone(),
                            reason: "the operator took the task over to inspect it".to_owned(),
                        },
                        TaskHistoryEvent::OperatorSignalResolved {
                            signal_id: deferred.signal.id,
                            resolved: true,
                            by: TaskOperatorSignalAuthor::Operator,
                        },
                    ],
                )
                .await?;
        }
        if !task.facts.sandbox_available {
            task = self.provision(task).await?;
        }
        let engine = self.engine(&task)?;
        let conversation_id = self
            .agent_conversation(&task, engine.operator_requires_conversation())
            .await?;
        let command = engine.operator_command(&conversation_id);
        let mut bound = None;
        let command = match task.facts.sandbox_id.as_deref() {
            Some(sandbox_id) => match self.bind_ports(sandbox_id) {
                Ok(()) => {
                    bound = Some(sandbox_id.to_owned());
                    with_helper(&command)
                }
                Err(error) => {
                    tracing::warn!(%error, "a taken-over task's ports stay in its sandbox");
                    command
                }
            },
            None => command,
        };
        let launched = self
            .launch(task, command, TaskExecutionMode::Operator, None)
            .await;
        if launched.is_err()
            && let Some(sandbox_id) = bound
        {
            self.port_relays.release(&sandbox_id);
        }
        launched
    }
    pub(super) async fn launch(
        &self,
        task: Task,
        command: String,
        mode: TaskExecutionMode,
        answering_signal_id: Option<String>,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        if task.facts.accepted {
            return Err(TaskServiceError::Task(super::TaskError::OperationRejected(
                "this task has already been accepted",
            )));
        }
        let sandbox_id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let agent_run = TaskAgentRun {
            id: Uuid::new_v4().to_string(),
            task_id: task.id.clone(),
            answering_signal_id,
        };
        let events = vec![TaskHistoryEvent::AgentRunStarted {
            execution_status: TaskExecutionStatus::Running,
            agent_run_id: agent_run.id.clone(),
            sandbox_id: sandbox_id.clone(),
            execution_mode: mode,
            answering_signal_id: agent_run.answering_signal_id.clone(),
        }];
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
        let snapshot_path = self.suspend_capsule(task).await?;
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
                        snapshot_path,
                    },
                ],
            )
            .await?;
        // The sandbox is gone, so nothing can reach the channel until the task
        // is provisioned again.
        self.release_agent_channel(&task.id);
        Ok(task)
    }

    /// Hold a wrap-up request while the host watches the linked pull requests,
    /// and give the task's capsule back to the host in the meantime.
    ///
    /// The run ends here rather than when the wait resolves: it is the run's
    /// request that is held, and the report turn is already over. Recording the
    /// pause with it is what makes the wait survive a restart without an agent
    /// turn.
    pub(super) async fn hold_review_checks(
        &self,
        task: &Task,
        agent_run: Option<&TaskAgentRun>,
    ) -> Result<Task, TaskServiceError> {
        let snapshot_path = self.suspend_capsule(task).await?;
        let mut events = Vec::new();
        if let Some(agent_run) = agent_run {
            events.push(run_ended(
                agent_run,
                TaskAgentRunOutcome::AwaitingOperator,
                Some(0),
            ));
        }
        events.push(TaskHistoryEvent::ExecutionPaused {
            execution_status: TaskExecutionStatus::Paused,
            sandbox_available: false,
            snapshot_available: true,
            execution_pause_reason: TaskExecutionPauseReason::AwaitingReviewChecks,
            snapshot_path,
        });
        let task = self.record_snapshot(task, &events).await?;
        self.release_agent_channel(&task.id);
        Ok(task)
    }

    /// Stop a task's capsule and answer with where its snapshot landed.
    async fn suspend_capsule(&self, task: &Task) -> Result<String, TaskServiceError> {
        let sandbox_id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        match self
            .isolation
            .suspend(
                sandbox_id,
                snapshot_name(&task.id),
                self.snapshots_dir.clone(),
            )
            .await
        {
            Ok(path) => Ok(path.to_string_lossy().into_owned()),
            Err(error) => {
                self.fail(task, error.to_string()).await?;
                Err(TaskServiceError::Isolation(error))
            }
        }
    }

    /// Make the sandbox that observes a provider for one credential owner.
    ///
    /// The base image plus what the providers install is the least that can
    /// read them, and the shell shape mounts the owner's own profile without
    /// making a task of it. It is left running for the next observation to reuse.
    pub(super) async fn open_watcher(&self, id: String) -> Result<(), TaskServiceError> {
        self.isolation
            .watcher(
                id,
                Project::profile_only("vcs-observer"),
                vcs::profile_recipes(),
                &Unwatched,
            )
            .await
            .map_err(TaskServiceError::Isolation)
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
