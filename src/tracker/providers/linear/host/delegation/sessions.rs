use super::*;

impl RemoteTracker {
    pub(in crate::tracker::providers::linear::host) async fn stop_for_terminated_session(
        &self,
        event: &SessionEvent,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        let Some(report) = self.report_for_session(&event.session_id) else {
            if event
                .creator_id
                .as_deref()
                .is_some_and(|author| event.is_from(author))
            {
                self.mutate(|ledger| {
                    ledger.terminated_sessions.push(TerminatedSession {
                        session_id: event.session_id.clone(),
                        terminated_at_ms: now_ms(),
                    })
                })?;
            }
            // The task that holds the issue is what the operator stopped, even when
            // this machine never got as far as reporting on it: a stop that only
            // records a session leaves work running that nobody is watching. Only the
            // delegation this task was founded in can stop it, because an issue has
            // newer sessions than the one the running task belongs to.
            if let Some(issue_id) = event.issue_id.as_deref()
                && let Some(task) = host.work_for_issue(issue_id.to_owned(), Vec::new()).await?
                && task.delegation.as_deref() == Some(event.session_id.as_str())
            {
                host.stop(
                    task.task_id.clone(),
                    TaskExecutionPauseReason::StoppedInTracker,
                )
                .await?;
                tracing::info!(
                    task = %task.task_id,
                    session = %event.session_id,
                    "a stopped session stopped the task it founded"
                );
            }
            return Ok(());
        };
        if !event.is_from(&report.tracker_user_id) {
            return Ok(());
        }
        host.stop(
            report.task_id.clone(),
            TaskExecutionPauseReason::StoppedInTracker,
        )
        .await?;
        // The work is gone, so the app has nothing left to be asked for: the issue
        // goes back to whoever owns it rather than pointing at a machine that
        // stopped listening.
        if let Err(error) = self
            .config
            .automation
            .release_issue(&self.issue_reference(&report.issue_id))
            .await
        {
            tracing::warn!(issue = %report.issue_id, %error, "could not release a stopped task's issue");
        }
        self.say_system(
            &report,
            &stable_id(&report.task_id, "stopped"),
            AgentActivityContent::Response {
                body: "Stopped. altitude stopped this task and handed the issue back; \
                       the work it had done is still in altitude, if you want it."
                    .to_owned(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        self.finish_report(&report)
    }

    pub(in crate::tracker::providers::linear::host) async fn follow_issue_state(
        &self,
        issue_id: &str,
        state_id: &str,
        actor_id: &str,
        occurred_at_ms: i64,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        let Some(task) = host.work_for_issue(issue_id.to_owned(), Vec::new()).await? else {
            return Ok(());
        };
        if self.user_for_owner(&task.owner).as_deref() != Some(actor_id) {
            return Ok(());
        }
        let delegation = self
            .config
            .automation
            .issue_delegation(issue_id)
            .await
            .map_err(|error| error.to_string())?;
        if delegation.state.id != state_id {
            return Ok(());
        }
        // A task this machine is not reporting on has said nothing on the issue, so
        // there is no status to forget and no thread to end: the ending is the
        // operator's word alone, and all this machine has to do is stop treating the
        // issue as work a later sweep could pick up.
        let Some(report) = self.report_for_task(&task.task_id) else {
            if matches!(
                delegation.state.state_type.as_str(),
                "completed" | "canceled"
            ) {
                self.settle(issue_id)?;
            }
            return Ok(());
        };
        if let Err(error) = self.forget_status(&report.issue_id) {
            tracing::warn!(issue = %report.issue_id, %error, "could not forget the issue's status");
        }
        let is_done = delegation.state.state_type == "completed";
        if !is_done && delegation.state.state_type != "canceled" {
            return Ok(());
        }
        self.settle(&report.issue_id)?;
        let review = if is_done {
            self.waiting_review(&report, occurred_at_ms, host).await?
        } else {
            None
        };
        if let Some(review) = review {
            host.accept(report.task_id.clone(), review.id.clone())
                .await?;
            let events = host.events(report.task_id.clone()).await?;
            return self.finish_accepted(host, &report, &events).await;
        }
        self.stop_for_ending(host, &report, is_done).await
    }

    pub(in crate::tracker::providers::linear::host) async fn waiting_review(
        &self,
        report: &Cursor,
        occurred_at_ms: i64,
        host: &dyn DelegatedWork,
    ) -> Result<Option<TaskAsk>, String> {
        let was_waiting = host
            .events(report.task_id.clone())
            .await?
            .iter()
            .rev()
            .filter(|(at_ms, _)| *at_ms <= occurred_at_ms)
            .find_map(|(_, event)| match event {
                TaskHistoryEvent::AgentRunStarted { .. }
                | TaskHistoryEvent::ExecutionQueued { .. } => Some(false),
                TaskHistoryEvent::ExecutionPaused {
                    execution_pause_reason,
                    ..
                }
                | TaskHistoryEvent::TransferLanded {
                    execution_pause_reason,
                    ..
                } => Some(*execution_pause_reason == TaskExecutionPauseReason::AwaitingOperator),
                _ => None,
            })
            .unwrap_or(false);
        if !was_waiting {
            return Ok(None);
        }
        Ok(host
            .signal_threads(report.task_id.clone())
            .await?
            .into_iter()
            .filter(TaskAsk::is_unresolved)
            .find(|signal| {
                signal.kind == TaskAskKind::ReadyForReview
                    && signal.last_raised_at_ms <= occurred_at_ms
            }))
    }

    pub(in crate::tracker::providers::linear::host) async fn stop_for_ending(
        &self,
        host: &dyn DelegatedWork,
        report: &Cursor,
        is_done: bool,
    ) -> Result<(), String> {
        let notice = if is_done {
            "You moved this issue to Done, so altitude stopped the task: no more work is asked \
             for. Raise a new task when you want more from it."
        } else {
            "You canceled this issue, so altitude stopped the task."
        };
        if is_done {
            // Done is the operator's ending, the same as accepting the work: the
            // task is finished rather than paused for someone to interpret.
            host.complete(report.task_id.clone()).await?;
        } else {
            host.stop(
                report.task_id.clone(),
                TaskExecutionPauseReason::TrackerEnded,
            )
            .await?;
        }
        if let Err(error) = self
            .say_system(
                report,
                &stable_id(&report.task_id, "closed-by-operator"),
                AgentActivityContent::Response {
                    body: notice.to_owned(),
                },
            )
            .await
        {
            tracing::warn!(task = %report.task_id, %error, "could not say why a task stopped");
        }
        self.finish_report(report)
    }
}
