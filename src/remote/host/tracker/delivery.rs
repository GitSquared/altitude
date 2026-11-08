use super::ledger::{Cursor, Delivery, StoredEvent};
use crate::{
    task::{TaskAsk, TaskAskAuthor, TaskExecutionPauseReason, TaskHistoryEvent},
    tracker::{
        TrackerStatus,
        providers::linear::automation::{AgentActivityContent, CommentSite},
    },
};
use std::collections::HashSet;
use std::time::Instant;

use super::*;

impl RemoteTracker {
    pub(super) fn enqueue(&self, key: String, event: StoredEvent) -> Result<bool, String> {
        let now = now_ms();
        let is_new = self.mutate(|ledger| {
            ledger.prune(now);
            if ledger.deliveries.iter().any(|delivery| delivery.key == key) {
                return false;
            }
            ledger.deliveries.push(Delivery {
                key: key.clone(),
                received_at_ms: now,
                next_attempt_at_ms: now,
                done: false,
                event,
            });
            true
        })?;
        if is_new {
            self.notify.notify_one();
        }
        Ok(is_new)
    }

    pub(super) fn pending(&self) -> Vec<Delivery> {
        self.lock()
            .map(|ledger| {
                ledger
                    .deliveries
                    .iter()
                    .filter(|delivery| !delivery.done)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn complete_delivery(&self, key: &str) -> Result<(), String> {
        self.mutate(|ledger| {
            if let Some(delivery) = ledger.deliveries.iter_mut().find(|d| d.key == key) {
                delivery.done = true;
            }
            ledger.prune(now_ms());
        })
    }

    pub(super) fn defer_delivery(&self, key: &str) -> Result<(), String> {
        let retry_at = now_ms() + RETRY.as_millis() as i64;
        self.mutate(|ledger| {
            if let Some(delivery) = ledger.deliveries.iter_mut().find(|d| d.key == key) {
                delivery.next_attempt_at_ms = retry_at;
            }
        })
    }

    // Answers when the next delivery is due to be tried again, if one is waiting on a
    // retry rather than on this machine doing something.
    pub(super) async fn process_deliveries(&self, host: &dyn Host) -> Option<Instant> {
        let mut retry = None;
        for delivery in self.pending() {
            if delivery.next_attempt_at_ms > now_ms() {
                let at = Instant::now()
                    + Duration::from_millis((delivery.next_attempt_at_ms - now_ms()) as u64);
                retry = Some(retry.map_or(at, |held: Instant| held.min(at)));
                continue;
            }
            match self.apply_delivery(&delivery.event, host).await {
                Ok(()) => {
                    if let Err(error) = self.complete_delivery(&delivery.key) {
                        tracing::error!(delivery = %delivery.key, %error, "could not record a completed Linear delivery; it stays queued");
                    }
                }
                Err(error) => {
                    tracing::warn!(delivery = %delivery.key, %error, "a Linear delivery will be retried");
                    if let Err(persist_error) = self.defer_delivery(&delivery.key) {
                        tracing::error!(delivery = %delivery.key, %persist_error, "could not record a deferred Linear delivery");
                    }
                    retry = Some(Instant::now() + RETRY);
                }
            }
        }
        retry
    }

    pub(super) async fn apply_delivery(
        &self,
        event: &StoredEvent,
        host: &dyn Host,
    ) -> Result<(), String> {
        match event {
            StoredEvent::Session(session) => {
                if session.is_stop() {
                    return self.stop_for_terminated_session(session, host).await;
                }
                match session.action.as_str() {
                    "created" => self.delegate_new_session(session, host).await,
                    "prompted" => self.answer_prompt(session, host).await,
                    _ => Ok(()),
                }
            }
            StoredEvent::IssueStateChanged(change) => {
                self.follow_issue_state(
                    &change.issue_id,
                    &change.state_id,
                    &change.actor_id,
                    change.occurred_at_ms,
                    host,
                )
                .await
            }
            StoredEvent::Comment(comment) => self.answer_comment(comment, host).await,
            StoredEvent::Reaction(reaction) => self.answer_reaction(reaction, host).await,
        }
    }

    pub(super) async fn reopen_closed_thread(
        &self,
        report: &Cursor,
        site: &CommentSite,
        host: &dyn Host,
    ) {
        let root = site.root_comment_id.as_str();
        let Ok(threads) = host.signal_threads(report.task_id.clone()).await else {
            return;
        };
        let Some(thread) = threads.iter().find(|thread| {
            thread.is_resolved() && published_comment_id(&report.task_id, &thread.id) == root
        }) else {
            return;
        };
        if let Err(error) = host
            .reopen_signal(report.task_id.clone(), thread.id.clone())
            .await
        {
            tracing::warn!(task = %report.task_id, signal = %thread.id, %error, "could not reopen a closed thread");
            return;
        }
        tracing::info!(
            task = %report.task_id,
            signal = %thread.id,
            "a reply reopened a closed thread"
        );
    }

    pub(super) fn refused_event(
        &self,
        report: &Cursor,
        index: usize,
        error: String,
    ) -> Result<(), String> {
        let attempts = if report.failed_at == Some(index) {
            report.failed_attempts + 1
        } else {
            1
        };
        if attempts < EVENT_ATTEMPTS {
            self.mutate(|ledger| {
                if let Some(held) = ledger
                    .cursors
                    .iter_mut()
                    .find(|held| held.task_id == report.task_id)
                {
                    held.failed_at = Some(index);
                    held.failed_attempts = attempts;
                }
            })?;
            return Err(error);
        }
        tracing::warn!(
            task = %report.task_id,
            index,
            %error,
            "skipping an event the tracker kept refusing"
        );
        self.mutate(|ledger| {
            if let Some(held) = ledger
                .cursors
                .iter_mut()
                .find(|held| held.task_id == report.task_id)
            {
                held.failed_at = None;
                held.failed_attempts = 0;
            }
        })?;
        self.advance_cursor(&report.task_id, index + 1)
    }

    pub(super) async fn report_event(
        &self,
        host: &dyn Host,
        report: &Cursor,
        events: &[(i64, TaskHistoryEvent)],
        index: usize,
        unresolved: &HashSet<String>,
    ) -> Result<Option<()>, String> {
        let (_, event) = &events[index];
        match event {
            TaskHistoryEvent::AgentModelObserved { .. } => {
                if let Ok(Some(session_agent)) = host.session_agent(report.task_id.clone()).await
                    && let Err(error) = self
                        .summarise_session(
                            &report.session,
                            &report.tracker_user_id,
                            Some(&session_agent),
                        )
                        .await
                {
                    tracing::warn!(task = %report.task_id, %error, "could not update the session model label");
                }
            }
            // A pause is why the session stops looking like it is thinking. The
            // tracker shows the last thing told to it, and a machine that
            // restarted or an agent waiting on a reply used to leave it saying
            // "working" forever.
            TaskHistoryEvent::ExecutionPaused {
                execution_pause_reason,
                ..
            } => {
                self.flush_speech(report).await;
                let id = stable_id(&format!("{}-{index}", report.task_id), "paused");
                // Per pause, not per task: a thread that says nothing the second time
                // it waits is a thread the operator thinks is still thinking.
                let content = if waits_on_a_person(execution_pause_reason) {
                    let asking = events[..=index]
                        .iter()
                        .rev()
                        .find_map(|(_, event)| match event {
                            TaskHistoryEvent::OperatorSignalRaised { signal }
                                if unresolved.contains(&signal.id) =>
                            {
                                Some(signal.kind)
                            }
                            _ => None,
                        });
                    let operator = self.operator_name(&report.tracker_user_id).await;
                    // The last thing a run says is the handoff, and it is said to a
                    // person: it is what leaves the session waiting on them rather than
                    // ticking as if the work were still going on.
                    AgentActivityContent::Response {
                        body: pause_message(execution_pause_reason, &operator, asking),
                    }
                } else {
                    AgentActivityContent::Thought {
                        body: format!("⛰ {}", execution_pause_reason.notice()),
                    }
                };
                self.say(report, &id, content)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            // A task that stopped is not a task that is working. The thread the
            // failure raised carries the detail; this is what stops the session
            // claiming the agent is still busy, which is what it looked like while
            // the machine had already given up.
            TaskHistoryEvent::ExecutionFailed { error, .. } => {
                self.flush_speech(report).await;
                self.say(
                    report,
                    &stable_id(&format!("{}-{index}", report.task_id), "failed"),
                    AgentActivityContent::Error {
                        body: failure_notice(error),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            TaskHistoryEvent::PlanUpdated { steps } => {
                self.set_plan(report, steps)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            TaskHistoryEvent::AgentRunStarted { agent_run_id, .. } => {
                let previous_run = events[..index].iter().rposition(|(_, event)| {
                    matches!(event, TaskHistoryEvent::AgentRunStarted { .. })
                });
                match previous_run {
                    // The session's first run, and the one activity that replaces
                    // whatever was said while the task waited: a queue notice is
                    // ephemeral, so it stays on screen until something else is said.
                    None => {
                        let agent = host
                            .session_agent(report.task_id.clone())
                            .await
                            .ok()
                            .flatten();
                        let operator = self
                            .config
                            .automation
                            .user_name(&report.tracker_user_id)
                            .await
                            .ok();
                        self.say(
                            report,
                            &stable_id(&report.task_id, "picked-up"),
                            AgentActivityContent::Thought {
                                body: picking_up(operator.as_deref(), agent.as_deref()),
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    }
                    // A run answers the reply that arrived after the last one. A message
                    // from before that has already been answered, and saying so again on
                    // every later run repeats a line the operator has already read.
                    Some(previous_run)
                        if events[previous_run + 1..index].iter().any(|(_, event)| {
                            matches!(
                                event,
                                TaskHistoryEvent::OperatorSignalMessage {
                                    author: TaskAskAuthor::Operator,
                                    ..
                                }
                            )
                        }) =>
                    {
                        self.say(
                            report,
                            &stable_id(&report.task_id, agent_run_id),
                            AgentActivityContent::Thought {
                                body: "Got your reply. Altitude is continuing the task.".into(),
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    }
                    Some(_) => {}
                }
                self.write_status(host, report, TrackerStatus::InProgress)
                    .await?;
            }
            // What the agent says in a thread belongs to the thread. Until this, a reply
            // lived only in the task's history: whoever read the issue saw the question
            // and never the answer.
            TaskHistoryEvent::OperatorSignalMessage {
                signal_id,
                author: TaskAskAuthor::Agent,
                body,
            } => {
                self.post_signal_reply(report, signal_id, body).await;
            }
            TaskHistoryEvent::OperatorSignalRaised { signal } => {
                if unresolved.contains(&signal.id) {
                    let contributions = contribution_details(&events[..=index]);
                    if let Some(status) = self
                        .publish_signal(
                            report,
                            &signal.id,
                            signal.kind,
                            &signal.title,
                            &signal.body,
                            &contributions,
                        )
                        .await?
                    {
                        self.write_status(host, report, status).await?;
                    }
                }
            }
            TaskHistoryEvent::OperatorSignalResolved {
                signal_id,
                resolved: true,
                ..
            } => {
                self.follow_signal_thread(report, signal_id, true).await;
                // Answering a question puts the work back in progress: the issue
                // said In Review while the task was waiting, and it is not waiting
                // any more. An issue whose ending was already written is left as
                // the operator left it.
                self.say_task_is_running(host, report).await;
            }
            TaskHistoryEvent::OperatorSignalResolved {
                signal_id,
                resolved: false,
                ..
            } => {
                // Reopening a thread says the question is live again, so the
                // tracker shows it open and the issue is waiting on the operator.
                self.follow_signal_thread(report, signal_id, false).await;
                if !self.is_settled(&report.issue_id)
                    && let Err(error) = self
                        .write_status(host, report, TrackerStatus::InReview)
                        .await
                {
                    tracing::warn!(issue = %report.issue_id, %error, "could not say the task is waiting again");
                }
            }
            TaskHistoryEvent::OperatorSignalConfirmed { signal_id } => {
                self.flush_speech(report).await;
                self.follow_signal_thread(report, signal_id, true).await;
                self.finish_accepted(host, report, &events[..=index])
                    .await?;
                return Ok(Some(()));
            }
            TaskHistoryEvent::TaskCompleted { .. } => {
                self.flush_speech(report).await;
                self.resolve_signal_comments(report, &events[..=index])
                    .await;
                self.finish_accepted(host, report, &events[..=index])
                    .await?;
                return Ok(Some(()));
            }
            _ => {}
        }
        Ok(None)
    }

    pub(super) async fn sync_report(&self, host: &dyn Host, report: &Cursor) -> Result<(), String> {
        let events = host.events(report.task_id.clone()).await?;
        // A cursor past the end means the history it counted is gone, which is not a
        // reason to replay from the beginning: a replay re-writes statuses and
        // re-resolves threads the operator has since touched themselves.
        if report.reported_events > events.len() {
            self.advance_cursor(&report.task_id, events.len())?;
            return Ok(());
        }
        // Signals still open are the only ones worth publishing; one already
        // answered or confirmed must not be raised again on the next pass.
        let unresolved: HashSet<String> = host
            .signal_threads(report.task_id.clone())
            .await?
            .into_iter()
            .filter(TaskAsk::is_unresolved)
            .map(|signal| signal.id)
            .collect();
        // A report that only began after a transfer must not replay the source's
        // whole life as new activity. Folding leaves the projected state below.
        let folded = report.reported_events == 0 && was_transferred(&events);
        let start = if folded {
            events.len()
        } else {
            report.reported_events
        };
        for index in start..events.len() {
            let outcome = self
                .report_event(host, report, &events, index, &unresolved)
                .await;
            match outcome {
                Ok(Some(done)) => return Ok(done),
                Ok(None) => {}
                Err(error) => return self.refused_event(report, index, error),
            }
            self.advance_cursor(&report.task_id, index + 1)?;
        }
        if folded {
            self.replay_signals(host, report, &events).await?;
        }
        Ok(())
    }

    pub(super) fn unhook_held(&self, report: &Cursor) -> Result<bool, String> {
        let now = now_ms();
        let mut held_for = None;
        self.mutate(|ledger| {
            if let Some(held) = ledger
                .cursors
                .iter_mut()
                .find(|held| held.task_id == report.task_id)
            {
                let at = *held.unhooked_at.get_or_insert(now);
                held_for = Some(now - at);
            }
        })?;
        Ok(held_for.is_some_and(|held_for| held_for >= UNHOOK_GRACE_MS))
    }

    pub(super) fn clear_unhook(&self, report: &Cursor) -> Result<(), String> {
        if report.unhooked_at.is_none() {
            return Ok(());
        }
        self.mutate(|ledger| {
            if let Some(held) = ledger
                .cursors
                .iter_mut()
                .find(|held| held.task_id == report.task_id)
            {
                held.unhooked_at = None;
            }
        })
    }

    pub(super) async fn unhook(&self, host: &dyn Host, report: &Cursor) {
        if host
            .stop(
                report.task_id.clone(),
                TaskExecutionPauseReason::DelegationWithdrawn,
            )
            .await
            .is_err()
        {
            return;
        }
        if let Err(error) = self
            .say(
                report,
                &stable_id(&report.task_id, "unhooked"),
                AgentActivityContent::Response {
                    body:
                        "This issue isn't delegated to Altitude anymore, so the task was stopped."
                            .to_owned(),
                },
            )
            .await
        {
            tracing::warn!(task = %report.task_id, %error, "could not say why a task was unhooked");
            return;
        }
        if let Err(error) = self.finish_report(report) {
            tracing::warn!(task = %report.task_id, %error, "could not close an unhooked report");
        }
    }

    pub(super) async fn poll_reports(&self, host: &dyn Host) {
        for report in self.cursors().into_iter().filter(|report| !report.finished) {
            // A task the operator removed is no longer work: stop writing about
            // it rather than reporting a history that will never advance. A task
            // that left because it moved elsewhere still gets one goodbye, and
            // neither ending stops the work or marks the issue Done.
            match host.is_kept(report.task_id.clone()).await {
                Ok(true) => {}
                Ok(false) => {
                    let moved = host
                        .events(report.task_id.clone())
                        .await
                        .unwrap_or_default()
                        .iter()
                        .any(|(_, event)| {
                            matches!(event, TaskHistoryEvent::TransferCommitted { .. })
                        });
                    let outcome = if moved {
                        self.finish_transferred(&report).await
                    } else {
                        self.finish_report(&report)
                    };
                    if let Err(error) = outcome {
                        tracing::warn!(task = %report.task_id, %error, "could not drop a removed task's report");
                    }
                    continue;
                }
                Err(error) => {
                    tracing::warn!(task = %report.task_id, %error, "could not tell whether a task is still here");
                    continue;
                }
            }
            // Whether this work came from a delegation is a fact about the task, not
            // a second record beside it: a delegation's work ends when the issue
            // stops being delegated, and a client's own work does not.
            let delegated = host
                .delegation_of(report.task_id.clone())
                .await
                .ok()
                .flatten()
                == Some(report.session.clone());
            if delegated {
                let delegation = match self
                    .config
                    .automation
                    .issue_delegation(&report.issue_id)
                    .await
                {
                    Ok(delegation) => delegation,
                    Err(error) => {
                        tracing::warn!(issue = %report.issue_id, %error, "could not read the issue's delegation");
                        continue;
                    }
                };
                if delegation.delegate_id.as_deref() != Some(self.config.automation.app_user_id()) {
                    // Un-delegating and delegating again says the operator changed
                    // their mind, not that the work stopped. Waiting before acting
                    // keeps a flap from ending a task and starting a second one.
                    match self.unhook_held(&report) {
                        Ok(true) => {
                            self.unhook(host, &report).await;
                            continue;
                        }
                        Ok(false) => continue,
                        Err(error) => {
                            tracing::warn!(task = %report.task_id, %error, "could not record a lapsed delegation");
                            continue;
                        }
                    }
                }
                if let Err(error) = self.clear_unhook(&report) {
                    tracing::warn!(task = %report.task_id, %error, "could not clear a lapsed delegation");
                }
            }
            if let Err(error) = self.sync_report(host, &report).await {
                tracing::warn!(task = %report.task_id, %error, "task updates remain queued");
            }
        }
    }
}
