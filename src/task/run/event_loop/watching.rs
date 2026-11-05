use std::sync::Arc;

use crate::{
    agent::{AgentRefusal, TaskStreamDecoder},
    isolation::{CommandEvent, CommandSession, IsolationEngine},
    task::mirror::review::Look,
    task::{
        Task, TaskAgentRun, TaskExecutionMode, TaskExecutionPauseReason, TaskHistoryEvent,
        TaskServiceError,
    },
    vcs::{CodeContribution, ContributionObservation},
};

use super::{ActiveRun, TaskEventLoop, TaskExecutionUpdate};

impl<E> TaskEventLoop<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub(super) fn watch_agent_run(
        &self,
        result: Result<(Task, TaskAgentRun, CommandSession), TaskServiceError>,
    ) {
        let (task, run, mut session) = match result {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(error = %error, "task failed to start");
                let _ = self.events.send(TaskExecutionUpdate::Started(Err(error)));
                return;
            }
        };
        self.terminal_output
            .lock()
            .expect("terminal output lock poisoned")
            .remove(&task.id);
        let _ = self
            .events
            .send(TaskExecutionUpdate::Started(Ok(task.clone())));
        self.runs.lock().expect("task run lock poisoned").insert(
            task.id.clone(),
            ActiveRun {
                commands: session.commands.clone(),
                agent_run_id: run.id.clone(),
                suspending: false,
            },
        );

        let mode = task.facts.execution_mode;
        let service = self.service.clone();
        let events = self.events.clone();
        let current_activity = Arc::clone(&self.current_activity);
        let event_loop = self.clone();
        tokio::spawn(async move {
            let mut stream = match mode {
                TaskExecutionMode::Autopilot => service.stream_decoder(&task),
                _ => None,
            };
            let mut progress = match mode {
                TaskExecutionMode::Autopilot => service.progress_decoder(&task),
                _ => None,
            };
            loop {
                let event = session.events.recv().await.unwrap_or_else(|| {
                    CommandEvent::Failed(
                        "command event channel closed without an exit status".into(),
                    )
                });
                match event {
                    CommandEvent::Output {
                        stream: output,
                        bytes,
                    } => {
                        if let Some(decoder) = &mut stream {
                            decoder.process(&bytes);
                        }
                        if let Some(decoder) = &mut progress
                            && let Some(activity) = {
                                decoder.process(&bytes);
                                decoder.take_activity()
                            }
                        {
                            current_activity
                                .lock()
                                .expect("activity lock poisoned")
                                .insert(task.id.clone(), activity.clone());
                            let _ = events.send(TaskExecutionUpdate::Activity {
                                task_id: task.id.clone(),
                                activity,
                            });
                        }
                        event_loop.cache_terminal_output(&task.id, &bytes);
                        let _ = events.send(TaskExecutionUpdate::Output {
                            task_id: task.id.clone(),
                            stream: output,
                            bytes,
                        });
                    }
                    event => {
                        // This run's entry, its control request and its suspension
                        // reason are taken together: all three describe the run
                        // that is ending now and mean nothing to the next one.
                        let suspending = event_loop
                            .take_active_run(&task.id)
                            .is_some_and(|active| active.suspending);
                        let control = event_loop.take_control_request(&task.id, &run.id);
                        service.release_ports(&task);
                        if suspending {
                            tracing::debug!(task_id = %task.id, "command ended during intentional suspension");
                            return;
                        }
                        if let Err(error) = event_loop
                            .settle_finished_run(task.clone(), &run, mode, control, stream, event)
                            .await
                        {
                            let result = service
                                .finish(&task, &run, CommandEvent::Failed(error.to_string()), None)
                                .await;
                            let _ = events.send(TaskExecutionUpdate::Finished(task.id, result));
                        }
                        // A run ending is the main way a slot frees.
                        event_loop.admit();
                        return;
                    }
                }
            }
        });
    }

    pub(super) async fn observe_owner(&self, owner: &str, tasks: &[&Task]) {
        // A capsule that outlived its hold is given back before observing, so
        // watching never competes with running work.
        let mut watching = Vec::with_capacity(tasks.len());
        for task in tasks {
            if !task.facts.sandbox_available {
                watching.push((*task).clone());
                continue;
            }
            match self.service.hold_review_checks(task, None).await {
                Ok(task) => watching.push(task),
                Err(error) => {
                    tracing::warn!(task_id = %task.id, %error, "could not release a task capsule while watching its pull requests");
                }
            }
        }
        // A pull request linked to two reviews is one pull request, asked about
        // once.
        let mut contributions: Vec<CodeContribution> = Vec::new();
        for task in &watching {
            for contribution in &task.code_contributions {
                if !contributions.iter().any(|seen| seen.id == contribution.id) {
                    contributions.push(contribution.clone());
                }
            }
        }
        let observations = match self.watchers.look(owner, &contributions).await {
            Look::Read(observations) => observations,
            // Nothing to decide from: either the provider was not asked, or it was
            // asked and could not be read. A provider that cannot be read must not
            // be mistaken for a finished check, and the owner's backoff decides
            // when either is tried again.
            Look::Unreadable | Look::NotAsked => return,
            // Only the operator can sign a provider in again, so the reviews this
            // owner is holding become theirs.
            Look::GivenUp => {
                let reason = self
                    .watchers
                    .last_failure(owner)
                    .await
                    .unwrap_or_else(|| "it did not say".to_owned());
                for task in &watching {
                    if let Some(deferred) = task.facts.deferred_review.clone() {
                        self.abandon_review(task, &deferred, &reason).await;
                    }
                }
                return;
            }
        };
        for task in &watching {
            self.decide_held_review(task, &observations).await;
        }
    }

    pub(super) async fn record_observations(
        &self,
        task: &Task,
        observations: &[ContributionObservation],
    ) -> Option<(Task, bool)> {
        match self
            .service
            .store
            .record_code_contribution_observations(&task.id, observations)
            .await
        {
            Ok(recorded) => Some(recorded),
            Err(error) => {
                tracing::warn!(task_id = %task.id, %error, "could not record PR observations");
                None
            }
        }
    }

    pub(super) async fn settle_finished_run(
        &self,
        task: Task,
        run: &TaskAgentRun,
        mode: TaskExecutionMode,
        control: Option<TaskExecutionMode>,
        mut stream: Option<Box<dyn TaskStreamDecoder>>,
        event: CommandEvent,
    ) -> Result<(), TaskServiceError> {
        // What the account looks like is recorded before the run is judged, because
        // the judgement reads it: a refusal is a spent window, not a sentence.
        let task = match stream.as_mut().and_then(|decoder| decoder.take_standing()) {
            Some(standing) => match self
                .service
                .record(
                    &task.id,
                    &[TaskHistoryEvent::AgentStandingReported { standing }],
                )
                .await
            {
                Ok(task) => task,
                Err(error) => {
                    tracing::warn!(task_id = %task.id, %error, "could not record an account's standing");
                    task
                }
            },
            None => task,
        };
        // What the run spent is recorded before anything is decided about it: a run
        // that ends in a pause or a refusal has still cost what it cost.
        let task = match stream.as_mut().and_then(|decoder| decoder.take_usage()) {
            Some(usage) => match self
                .service
                .record(&task.id, &[TaskHistoryEvent::from(&usage)])
                .await
            {
                Ok(task) => task,
                Err(error) => {
                    tracing::warn!(task_id = %task.id, %error, "could not record what a run cost");
                    task
                }
            },
            None => task,
        };
        // Which model a run used is decoded from its own stream now that no report
        // turn names it, and it is a reading of the run like what it cost.
        let task = match stream.as_mut().and_then(|decoder| decoder.take_model()) {
            Some(model) => match self
                .service
                .record(&task.id, &[TaskHistoryEvent::AgentModelObserved { model }])
                .await
            {
                Ok(task) => task,
                Err(error) => {
                    tracing::warn!(task_id = %task.id, %error, "could not record a run's model");
                    task
                }
            },
            None => task,
        };
        if let Some(target) = control {
            self.service.continue_turn(run).await?;
            // Handing control back ends the take over: the terminal closes before the
            // next run starts, so what the agent says next goes to the signals rather
            // than into a terminal the operator has already left.
            let _ = self.events.send(TaskExecutionUpdate::Finished(
                task.id.clone(),
                Ok(task.clone()),
            ));
            let result = match target {
                TaskExecutionMode::Autopilot => {
                    self.service.resume_autopilot_after_operator(task).await
                }
                TaskExecutionMode::Operator => self.service.start_operator(task).await,
            };
            let operator_task = result
                .as_ref()
                .ok()
                .filter(|(task, _, _)| task.facts.execution_mode == TaskExecutionMode::Operator)
                .map(|(task, _, _)| task.clone());
            self.watch_agent_run(result);
            if let Some(task) = operator_task {
                let _ = self.events.send(TaskExecutionUpdate::OperatorReady(task));
            }
            return Ok(());
        }
        if !matches!(event, CommandEvent::Exited(0)) {
            let refusal = stream.as_mut().and_then(|decoder| decoder.refusal());
            // A window that reopens on its own is the one ending that is not a
            // failure: the task keeps its place, and comes back when the clock runs
            // out rather than when the operator reads about it.
            if let Some(AgentRefusal::Window {
                window,
                clears_at_ms,
            }) = &refusal
            {
                let exit_code = match event {
                    CommandEvent::Exited(code) => Some(code),
                    _ => None,
                };
                let result = self
                    .service
                    .hold_for_limit(&task, run, window.clone(), *clears_at_ms, exit_code)
                    .await;
                let _ = self
                    .events
                    .send(TaskExecutionUpdate::Finished(task.id, result));
                return Ok(());
            }
            // What the account said is a better account of the ending than the exit
            // code is, and it is the only sentence a stopped task has to offer.
            let said = match &refusal {
                Some(AgentRefusal::Stopped { said }) => Some(said.as_str()),
                _ => None,
            };
            let diagnostic = said.or_else(|| {
                stream
                    .as_mut()
                    .and_then(|decoder| decoder.failure_message())
            });
            let result = self.service.finish(&task, run, event, diagnostic).await;
            let _ = self
                .events
                .send(TaskExecutionUpdate::Finished(task.id, result));
            return Ok(());
        }
        match mode {
            TaskExecutionMode::Autopilot => {
                // What a turn decided is what it said. A turn that left something
                // for the operator waits for them; one that left nothing is resumed,
                // because Altitude keeps a task going until the agent can say why it
                // should stop. Nothing is asked again: the signals are the answer,
                // and the ending each of these paths records is the run's own.
                let task = match self.service.store.get(&task.id).await {
                    Ok(Some(moved)) => moved,
                    _ => task,
                };
                if task.facts.deferred_review.is_some() {
                    // A review with pull requests to wait for: give the capsule back
                    // now, and let the host watch them.
                    let result = self.service.hold_review_checks(&task, Some(run)).await;
                    let _ = self
                        .events
                        .send(TaskExecutionUpdate::Finished(task.id, result));
                    return Ok(());
                }
                if task.facts.blocking_signal_kind.is_some() {
                    let result = self
                        .service
                        .await_operator(&task, run, TaskExecutionPauseReason::AwaitingOperator)
                        .await;
                    let _ = self
                        .events
                        .send(TaskExecutionUpdate::Finished(task.id, result));
                    return Ok(());
                }
                // A run of turns that each take one and say nothing between them
                // starts the next from the same state, so the operator is asked
                // instead of being left with an agent going in circles.
                if self.service.went_quiet(&task).await? {
                    let result = self
                        .service
                        .await_operator(&task, run, TaskExecutionPauseReason::AgentWentQuiet)
                        .await;
                    let _ = self
                        .events
                        .send(TaskExecutionUpdate::Finished(task.id, result));
                    return Ok(());
                }
                if !task.facts.pending_inputs.is_empty() {
                    tracing::info!(
                        task_id = %task.id,
                        messages = task.facts.pending_inputs.len(),
                        "a message arrived mid-turn; the next turn reads it"
                    );
                }
                self.service.continue_turn(run).await?;
                self.watch_agent_run(self.service.continue_autopilot(task).await);
            }
            // An operator's terminal ending is dropping the task off their desk,
            // which is the same thing as handing it back deliberately: the work
            // carries on under autopilot rather than the task being closed out.
            TaskExecutionMode::Operator => {
                self.service.continue_turn(run).await?;
                // Exiting the harness is leaving the terminal: the take over is over,
                // so the shell that was the operator's closes and the task carries on
                // headless. Without this the autopilot run that follows paints its
                // conversation into a terminal nobody is holding.
                let _ = self.events.send(TaskExecutionUpdate::Finished(
                    task.id.clone(),
                    Ok(task.clone()),
                ));
                // A task carried to an operator's machine goes back when they leave it.
                // Only an operator's machine can be the one holding it: a take over moves
                // work towards a person, and a remote has none. So the way home is
                // attached here or not at all, and where it is not, the task is simply
                // piloted where it is.
                if task.facts.arrived_from.is_some() && self.sends_home().is_some() {
                    self.hand_back(task.clone());
                    return Ok(());
                }
                // The next turn is told what happened to it, so the agent picks up from
                // the conversation rather than wondering why the operator went quiet.
                let _ = self
                    .service
                    .express_input(
                        &task.id,
                        "Altitude: the operator has left the task. You are back under \
                         Altitude's control; continue the work from wherever the \
                         conversation left it."
                            .to_owned(),
                    )
                    .await;
                self.watch_agent_run(self.service.resume_autopilot_after_operator(task).await);
            }
        }
        Ok(())
    }
}
