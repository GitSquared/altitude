use crate::{
    isolation::{
        CommandEvent, CommandSession, IsolationEngine, OutputStream, Unwatched, guest_absolute,
        shell_quote, with_port_relay,
    },
    project::Project,
    vcs::{ContributionObservation, ProviderBatch},
};
use std::time::Instant;
use uuid::Uuid;

// Generous on purpose: silent turns are a normal way to work, and this is the
// backstop for an agent out of things to do, not a step limit.
const TURNS_BEFORE_THE_OPERATOR: usize = 12;

use crate::{
    agent::AgentWindow,
    task::{
        IsolationError, Task, TaskAgentRun, TaskAgentRunOutcome, TaskAskAuthor, TaskAskKind,
        TaskError, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
        TaskHistoryEvent, TaskService, TaskServiceError, run_ended, snapshot_name,
    },
};

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone + 'static,
{
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
        self.start_autopilot_turn(task, "The operator has returned control to altitude. Review the preceding conversation and current workspace state, then continue the task autonomously wherever the original intent reasonably permits. Do not wait for routine confirmation.", None).await
    }
    // A shell in the sandbox the operator is working in. It is not a run and it says
    // nothing in the task's history: the agent is at its prompt while this is open,
    // and what the operator types here is theirs to look at, not a task event.
    pub(crate) async fn start_task_shell(
        &self,
        task: &Task,
    ) -> Result<CommandSession, TaskServiceError> {
        let sandbox_id = task
            .facts
            .sandbox_id
            .clone()
            .ok_or(TaskServiceError::Task(TaskError::MissingSandbox))?;
        let owner =
            task.operator_id()
                .ok_or(TaskServiceError::Task(TaskError::OperationRejected(
                    "the task records no operator to open a shell for",
                )))?;
        let isolation = self
            .isolation
            .for_operator(owner)
            .map_err(TaskServiceError::Isolation)?;
        // A debug shell belongs where the work is, and a login shell reads the
        // profile that put the toolchains on the path.
        let command = match task
            .project()
            .and_then(|project| project.checkout_path())
            .map(|checkout| guest_absolute(&checkout))
        {
            Some(Ok(checkout)) => format!("cd {} && exec bash -l", shell_quote(&checkout)),
            Some(Err(error)) => return Err(TaskServiceError::Isolation(error)),
            None => "exec bash -l".to_owned(),
        };
        isolation
            .run_command(sandbox_id, command)
            .await
            .map_err(TaskServiceError::Isolation)
    }

    pub(crate) async fn run_captured(
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
        // What it said when it failed is the only thing that explains why: a
        // provider that refuses without a reason leaves the operator guessing.
        let mut complaint = String::new();
        while let Some(event) = session.events.recv().await {
            match event {
                CommandEvent::Output {
                    stream: OutputStream::Stdout,
                    bytes,
                } => output.push_str(&String::from_utf8_lossy(&bytes)),
                CommandEvent::Output {
                    stream: OutputStream::Stderr,
                    bytes,
                } => complaint.push_str(&String::from_utf8_lossy(&bytes)),
                CommandEvent::Exited(0) => return Ok(output),
                CommandEvent::Exited(code) => {
                    let said = complaint.trim();
                    return Err(TaskServiceError::Isolation(IsolationError::new(
                        if said.is_empty() {
                            format!("observation command exited with code {code}")
                        } else {
                            format!("observation command exited with code {code}: {said}")
                        },
                    )));
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
    pub(crate) async fn observe_code_contributions(&self, task: Task) -> Task {
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
    pub(crate) async fn start_autopilot_turn(
        &self,
        task: Task,
        prompt: &str,
        // The signal thread this turn answers, when the operator started it by
        // replying to one.
        answering: Option<String>,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        // The record is not the runtime: a task whose capsule went away — a host
        // that restarted, a sandbox that could not be saved — is built again before
        // a turn reaches for its transcript or its files. Provisioning a task that
        // still has its capsule is one check and no work.
        let task = self.provision(task).await?;
        // Refresh what the contributions look like before the agent runs, so a
        // turn is planned against current external state rather than the state
        // as of the previous turn.
        let task = self.observe_code_contributions(task).await;
        let engine = self.engine(&task)?;
        let conversation_id = self.agent_conversation(&task, true).await?;
        let command = engine.autopilot_command(
            &conversation_id,
            prompt,
            &self.turn_context(&task, TaskExecutionMode::Autopilot),
            true,
        );
        self.launch(task, command, TaskExecutionMode::Autopilot, answering)
            .await
    }
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
                            by: TaskAskAuthor::Operator,
                            resolving_message_id: None,
                        },
                    ],
                )
                .await?;
        }
        // The machine's remaining questions were addressed to a person, and a person
        // is now at the terminal: a failure thread or a review thread left open would
        // stop the very next report as though nothing had been answered. An agent
        // that is still stuck asks again, with what it has learned from them.
        let mut answered = Vec::new();
        for thread in self.signal_threads(&task.id).await? {
            if !thread.is_unresolved()
                || !matches!(
                    thread.kind,
                    TaskAskKind::BlockingQuestion | TaskAskKind::ReadyForReview
                )
            {
                continue;
            }
            answered.push(TaskHistoryEvent::OperatorSignalResolved {
                signal_id: thread.id,
                resolved: true,
                by: TaskAskAuthor::Operator,
                resolving_message_id: None,
            });
        }
        if !answered.is_empty() {
            task = self.record(&task.id, &answered).await?;
        }
        let task = self.provision(task).await?;
        let engine = self.engine(&task)?;
        let conversation_id = self
            .agent_conversation(&task, engine.operator_requires_conversation())
            .await?;
        let command = engine.operator_command(
            &conversation_id,
            "altitude: the operator has taken this task over and will speak to you \
             directly from here. They are at the terminal now.",
        );
        let mut bound = None;
        let command = match task.facts.sandbox_id.as_deref() {
            Some(sandbox_id) => match self.bind_ports(sandbox_id) {
                Ok(()) => {
                    bound = Some(sandbox_id.to_owned());
                    with_port_relay(&command)
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
    pub(crate) async fn launch(
        &self,
        task: Task,
        command: String,
        mode: TaskExecutionMode,
        answering_signal_id: Option<String>,
    ) -> Result<(Task, TaskAgentRun, CommandSession), TaskServiceError> {
        if task.facts.accepted {
            return Err(TaskServiceError::Task(TaskError::OperationRejected(
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
        let launching = Instant::now();
        match self.isolation.run_command(sandbox_id, command).await {
            Ok(session) => {
                tracing::info!(
                    task = %task.id,
                    run = %agent_run.id,
                    elapsed_ms = launching.elapsed().as_millis() as u64,
                    mode = ?mode,
                    "the agent is running"
                );
                Ok((task, agent_run, session))
            }
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
    pub async fn went_quiet(&self, task: &Task) -> Result<bool, TaskServiceError> {
        let events = self.events(&task.id).await?;
        let mut turns = 0;
        for (_, event) in events.iter().rev() {
            match event {
                TaskHistoryEvent::AgentRunEnded {
                    outcome: TaskAgentRunOutcome::Continued,
                    ..
                } => turns += 1,
                // An update or a decision is the agent talking, not the task moving,
                // and it stops nothing: an agent that raises one every turn is
                // circling exactly like one that says nothing at all.
                TaskHistoryEvent::OperatorSignalRaised { signal }
                    if !matches!(
                        signal.kind,
                        TaskAskKind::BlockingQuestion | TaskAskKind::ReadyForReview
                    ) =>
                {
                    continue;
                }
                // The bookkeeping a turn leaves whether or not it did anything is
                // skipped; anything else is the task changing, which is what makes
                // the next turn a step rather than a repeat.
                TaskHistoryEvent::AgentRunStarted { .. }
                | TaskHistoryEvent::AgentRunEnded { .. }
                | TaskHistoryEvent::AgentModelObserved { .. }
                | TaskHistoryEvent::AgentUsageReported { .. }
                | TaskHistoryEvent::TrackerSynced { .. }
                | TaskHistoryEvent::ExecutionRecovered { .. } => continue,
                _ => break,
            }
            if turns >= TURNS_BEFORE_THE_OPERATOR {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn await_operator(
        &self,
        task: &Task,
        agent_run: &TaskAgentRun,
        reason: TaskExecutionPauseReason,
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
                        execution_pause_reason: reason,
                        snapshot_path,
                    },
                ],
            )
            .await?;
        Ok(task)
    }

    // The account's limit is a wait, not a loss: the capsule is given back so the
    // wait costs no room, and the task comes back on its own once the clock runs out.
    pub async fn hold_for_limit(
        &self,
        task: &Task,
        agent_run: &TaskAgentRun,
        window: AgentWindow,
        clears_at_ms: i64,
        exit_code: Option<i32>,
    ) -> Result<Task, TaskServiceError> {
        let snapshot_path = self.suspend_capsule(task).await?;
        let task = self
            .record_snapshot(
                task,
                &[
                    run_ended(agent_run, TaskAgentRunOutcome::Limited, exit_code),
                    TaskHistoryEvent::ExecutionPaused {
                        execution_status: TaskExecutionStatus::Paused,
                        sandbox_available: false,
                        snapshot_available: true,
                        execution_pause_reason: TaskExecutionPauseReason::AwaitingLimit,
                        snapshot_path,
                    },
                    TaskHistoryEvent::LimitHeld {
                        window,
                        clears_at_ms,
                    },
                ],
            )
            .await?;
        Ok(task)
    }

    pub(crate) async fn hold_review_checks(
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
        Ok(task)
    }

    async fn suspend_capsule(&self, task: &Task) -> Result<String, TaskServiceError> {
        // Everything this machine holds open in the guest is closed first. An agent
        // channel or a port relay is a session the guest is still serving, and a
        // guest with something to serve does not quiesce for a graceful stop — which
        // is what left a snapshot waiting on a shutdown that never came.
        self.release_agent_channel(&task.id);
        self.release_ports(task);
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
                self.fail(
                    task,
                    format!(
                        "could not save the task sandbox to {}: {error}",
                        self.snapshots_dir.display()
                    ),
                )
                .await?;
                Err(TaskServiceError::Isolation(error))
            }
        }
    }

    pub(crate) async fn open_watcher(
        &self,
        id: String,
        vcs_credential: &str,
    ) -> Result<(), TaskServiceError> {
        let recipe = self
            .credentials
            .recipe(vcs_credential)
            .map_err(TaskServiceError::Credentials)?;
        self.isolation
            .watcher(
                id,
                Project::profile_only("vcs-observer"),
                vec![recipe],
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
