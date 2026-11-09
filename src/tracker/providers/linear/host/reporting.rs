use super::ledger::Cursor;
use crate::tracker::{AskSurface, ask_marker};
use crate::{
    task::{TaskAskKind, TaskHistoryEvent, picture},
    tracker::{IssueReference, TrackerStatus, providers::linear::automation::AgentActivityContent},
};
use futures_util::future::join_all;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use tokio::time::timeout;

// Long enough for one request per bound task over a network that is about to go
// away, short enough to leave the guest's own shutdown most of its minute.
const SHUTDOWN_NOTICE_BUDGET: Duration = Duration::from_secs(10);

use crate::agent::ActivityKind;
use crate::brand;
use crate::isolation::Pressure;
use crate::task::TaskExecutionStatus;

use super::*;
use crate::task::TaskPlanStep;
use crate::tracker::TrackerError;
pub(super) struct QueueNotice {
    starting: bool,
    state: String,
    since: Instant,
    at: Instant,
}

impl RemoteTracker {
    // Everything this machine says in a session goes through one of these, so a write
    // that failed because the session has gone away is noticed where it happens: the
    // report is marked, and the next pass opens a session on the issue rather than
    // leaving the operator a thread that never says anything again.
    async fn write_activity(
        &self,
        report: &Cursor,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        let written = self
            .config
            .automation
            .create_activity(&report.session, activity_id, content)
            .await;
        if written.is_err() {
            self.break_session(&report.task_id);
        }
        written
    }

    pub(super) async fn say_system(
        &self,
        report: &Cursor,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        self.write_activity(report, activity_id, content.system())
            .await
    }

    pub(super) async fn say_agent(
        &self,
        report: &Cursor,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        self.write_activity(report, activity_id, content).await
    }

    pub(super) async fn say_system_ephemerally(
        &self,
        report: &Cursor,
        activity_id: &str,
        body: String,
    ) -> Result<(), TrackerError> {
        self.write_ephemeral(report, activity_id, brand::mark(body))
            .await
    }

    pub(super) async fn say_agent_ephemerally(
        &self,
        report: &Cursor,
        activity_id: &str,
        body: String,
    ) -> Result<(), TrackerError> {
        self.write_ephemeral(report, activity_id, body).await
    }

    async fn write_ephemeral(
        &self,
        report: &Cursor,
        activity_id: &str,
        body: String,
    ) -> Result<(), TrackerError> {
        let written = self
            .config
            .automation
            .create_ephemeral_thought(&report.session, activity_id, body)
            .await;
        if written.is_err() {
            self.break_session(&report.task_id);
        }
        written
    }

    // The paths that write into a session before a report exists still sign the
    // machine's words; they just have no report to mark broken if the write fails.
    pub(super) async fn create_system_activity(
        &self,
        session_id: &str,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        self.config
            .automation
            .create_activity(session_id, activity_id, content.system())
            .await
    }

    pub(super) async fn set_plan(
        &self,
        report: &Cursor,
        steps: &[TaskPlanStep],
    ) -> Result<(), TrackerError> {
        let written = self
            .config
            .automation
            .update_session_plan(&report.session, steps)
            .await;
        if written.is_err() {
            self.break_session(&report.task_id);
        }
        written
    }

    pub async fn start_reporting(
        &self,
        owner: &str,
        task_id: String,
        issue: IssueReference,
        delegation: Option<String>,
        session_agent: Option<String>,
    ) -> Result<(), String> {
        let Some(tracker_user_id) = self.user_for_owner(owner) else {
            return Err("sync a Linear account to this remote before delegating issue work".into());
        };
        // One cursor per task: the task is what a report is about, and a second
        // record keyed by the issue is what used to make two mechanisms disagree
        // about which task was current.
        if self.report_for_task(&task_id).is_some() {
            return Ok(());
        }
        // The session is the issue's, not the delegation's. An issue has one
        // conversation, and what the operator reads is that thread: a task that
        // replaces another on the same issue carries it on. A delegation's own session
        // is where the operator clicked, so it is used the first time and only when the
        // issue has nothing to carry.
        let candidate = match self.session_for_issue(&issue.id) {
            Some(session) => Some(session),
            None => delegation,
        };
        let session_id = match candidate {
            // One the operator or Linear has finished with takes nothing more: asking
            // first is the only way to know, because Linear accepts the activity and
            // then shows it nowhere.
            Some(candidate) => match self.config.automation.session_standing(&candidate).await {
                Ok(standing) if standing.can_receive => candidate,
                Ok(_) => self.open_session(&issue.id).await?,
                // A tracker that cannot be asked is not a reason to report nowhere, and
                // the session is as likely to be usable as not.
                Err(error) => {
                    tracing::debug!(session = %candidate, %error, "could not ask whether a session can receive");
                    candidate
                }
            },
            None => self.open_session(&issue.id).await?,
        };
        self.upsert_report(Cursor {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            session: session_id.clone(),
            owner: owner.to_owned(),
            tracker_user_id: tracker_user_id.clone(),
            task_id,
            reported_events: 0,
            finished: false,
            last_status: None,
            failed_at: None,
            failed_attempts: 0,
            unhooked_at: None,
            session_broken: false,
            activities: 0,
            activity_line: None,
        })?;
        // Work started on an issue is a new cycle: whatever ending was written for
        // the last task there does not bind this one, and the operator asked for
        // this by starting it.
        self.mutate(|ledger| ledger.settled_issues.retain(|id| id != &issue.id))?;
        if let Err(error) = self
            .summarise_session(&session_id, &tracker_user_id, session_agent.as_deref())
            .await
        {
            tracing::warn!(session = %session_id, %error, "could not name a piloted session");
        }
        Ok(())
    }

    pub async fn forget_task(&self, task_id: &str) {
        let Some(report) = self.report_for_task(task_id) else {
            return;
        };
        if let Err(error) = self
            .config
            .automation
            .release_issue(&self.issue_reference(&report.issue_id))
            .await
        {
            tracing::warn!(issue = %report.issue_id, %error, "could not release a deleted task's issue");
        }
        if let Err(error) = self
            .say_system(
                &report,
                &stable_id(task_id, "deleted"),
                AgentActivityContent::Response {
                    body: "This task was deleted, with its sandbox and its record.".to_owned(),
                },
            )
            .await
        {
            tracing::warn!(task = %task_id, %error, "could not say a task was deleted");
        }
        if let Err(error) = self.finish_report(&report) {
            tracing::warn!(task = %task_id, %error, "could not close a deleted task's report");
        }
    }

    // The machine is going down under this work, and a session that simply stops
    // reads as a hang to whoever is watching the issue. This says what is happening
    // before the process goes, and claims nothing about what survives: the stop is
    // best effort, and what happens to each task is decided by the boot after it.
    // Bounded, because the guest is shutting down and the tasks matter more.
    pub async fn announce_shutdown(&self, host: &dyn DelegatedWork) {
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::debug!(%error, "could not read bound tasks to say the machine is stopping");
                return;
            }
        };
        let shutdown = now_ms() / 1000;
        let automation = Arc::clone(&self.config.automation);
        let mut notices = Vec::new();
        for task in bound {
            let Some(report) = self.report_for_task(&task.task_id) else {
                continue;
            };
            // Only a run that is happening stops. A task waiting on the operator or one
            // still queued has nothing for the session to explain, and saying it does is
            // what makes the session contradict itself the next time the task speaks.
            if task.status != TaskExecutionStatus::Running {
                continue;
            }
            let session = report.session;
            let activity = stable_id(&report.task_id, &format!("shutdown:{shutdown}"));
            let automation = Arc::clone(&automation);
            notices.push(async move {
                // A status rather than a message: it says why the session goes quiet, and
                // the first thing this machine says when it is back replaces it.
                automation
                    .create_ephemeral_thought(
                        &session,
                        &activity,
                        brand::mark(
                            "altitude is shutting down, so this run stops here. The task is not \
                             finished: altitude picks it back up when the machine is back.",
                        ),
                    )
                    .await
            });
        }
        if notices.is_empty() {
            return;
        }
        match timeout(SHUTDOWN_NOTICE_BUDGET, join_all(notices)).await {
            Ok(results) => {
                for result in results.into_iter().filter_map(Result::err) {
                    tracing::debug!(%result, "could not say the machine is stopping");
                }
            }
            Err(_) => tracing::warn!(
                budget_s = SHUTDOWN_NOTICE_BUDGET.as_secs(),
                "saying the machine is stopping took too long"
            ),
        }
    }

    async fn open_session(&self, issue_id: &str) -> Result<String, String> {
        self.config
            .automation
            .create_session_on_issue(issue_id)
            .await
            .map_err(|error| error.to_string())
    }

    // A report already in play when this machine starts is checked once, on the way up:
    // Linear accepts activity for a session it has since finished with and shows it
    // nowhere, so a report can look healthy and be talking into a thread nobody has.
    // Checking on every pass would be asking Linear on a clock; asking once is enough
    // for the session a previous run left behind.
    pub(super) async fn verify_sessions(&self, host: &dyn DelegatedWork) {
        for report in self.cursors().into_iter().filter(|report| !report.finished) {
            match self
                .config
                .automation
                .session_standing(&report.session)
                .await
            {
                Ok(standing) if !standing.can_receive => self.replace_session(host, &report).await,
                Ok(standing) if standing.never_answered => {
                    self.introduce(host, &report).await;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(session = %report.session, %error, "could not ask whether a session can receive");
                }
            }
        }
    }

    // A session with a prompt this machine never answered sits in Linear as "thinking",
    // which says work is happening when the task may have been paused for hours. One
    // line says where it actually stands.
    async fn introduce(&self, host: &dyn DelegatedWork, report: &Cursor) {
        let Ok(bound) = host.bound_tasks().await else {
            return;
        };
        let Some(task) = bound.iter().find(|task| task.task_id == report.task_id) else {
            return;
        };
        let operator = self.operator_name(&report.tracker_user_id).await;
        let body = match (&task.status, task.pause_reason.as_ref()) {
            (TaskExecutionStatus::Running, _) => {
                format!(
                    "altitude is working on this task on {}.",
                    self.config.host_label
                )
            }
            (TaskExecutionStatus::Paused, Some(reason)) if waits_on_a_person(reason) => {
                pause_message(reason, &operator, task.ask)
            }
            (TaskExecutionStatus::Paused, Some(reason)) => reason.notice().to_owned(),
            (TaskExecutionStatus::Pending, _) => format!(
                "altitude has this task queued on {}; it starts when the machine has room.",
                self.config.host_label
            ),
            _ => format!(
                "altitude is reporting on this task from {}.",
                self.config.host_label
            ),
        };
        if let Err(error) = self
            .say_system(
                report,
                &stable_id(&report.task_id, &format!("introduced:{}", report.session)),
                AgentActivityContent::Response { body },
            )
            .await
        {
            tracing::debug!(task = %report.task_id, %error, "could not say where a task stands");
        }
    }

    async fn replace_session(&self, host: &dyn DelegatedWork, report: &Cursor) {
        let session = match self.open_session(&report.issue_id).await {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(task = %report.task_id, %error, "could not open a session for a report");
                return;
            }
        };
        if let Err(error) = self.move_session(&report.task_id, &session) {
            tracing::warn!(task = %report.task_id, %error, "could not move a report to a new session");
            return;
        }
        let agent = host
            .session_agent(report.task_id.clone())
            .await
            .ok()
            .flatten();
        if let Err(error) = self
            .summarise_session(&session, &report.tracker_user_id, agent.as_deref())
            .await
        {
            tracing::warn!(session = %session, %error, "could not name a new session");
        }
        tracing::info!(
            task = %report.task_id,
            issue = %report.issue_id,
            session = %session,
            "a session had gone away, so the report has a new one"
        );
        // A new session arrives with a prompt nobody has answered, and Linear says it is
        // thinking until something is said in it.
        self.introduce(host, report).await;
    }

    // A session can go away under a report — dismissed in Linear, or closed once the
    // work it carried ended — and a report writing into a thread nobody can read is the
    // silence this whole mechanism exists to prevent. Opening one on the issue and
    // moving there is cheap and rare, so it happens on the pass after the failure
    // rather than being retried where the write failed.
    pub(super) async fn repair_sessions(&self, host: &dyn DelegatedWork) {
        for report in self
            .cursors()
            .into_iter()
            .filter(|report| report.session_broken && !report.finished)
        {
            // A move that fails leaves the flag, so the next pass asks again rather
            // than the report going quiet for good.
            self.replace_session(host, &report).await;
        }
    }

    // Answers when a line that says nothing new becomes worth saying again, which is
    // the only clock this pass keeps: a task waiting in a queue would otherwise say
    // nothing at all until it moved.
    pub(super) async fn report_queue_standing(&self, host: &dyn DelegatedWork) -> Option<Instant> {
        let mut next = None;
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::debug!(%error, "could not read bound tasks to report queue standing");
                return None;
            }
        };
        // A machine reporting on nothing reads the queue not at all.
        if bound.is_empty() {
            return None;
        }
        let standings = match host.queue_standings().await {
            Ok(standings) => standings,
            Err(error) => {
                tracing::debug!(%error, "could not read the queue to report where tasks stand");
                return None;
            }
        };
        for task in bound {
            let Some(report) = self.report_for_task(&task.task_id) else {
                continue;
            };
            let Some(standing) = standings.get(&task.task_id) else {
                self.queue_notices
                    .lock()
                    .expect("queue notices lock poisoned")
                    .remove(&task.task_id);
                continue;
            };
            let since = self.queue_state_since(&task.task_id, standing);
            let state = queue_notice_state(standing);
            let body = queue_notice_line(&state, since);
            if !self.queue_notice_is_due(&task.task_id, standing.ahead, &state) {
                if let Some(at) = self.queue_notice_due_at(&task.task_id, standing.ahead, &state) {
                    next = Some(next.map_or(at, |held: Instant| held.min(at)));
                }
                continue;
            }
            let stamp = now_ms();
            let activity_id = stable_id(
                &report.session,
                &format!("queue-{stamp}-{}", standing.ahead),
            );
            if let Err(error) = self
                .say_system_ephemerally(&report, &activity_id, body)
                .await
            {
                tracing::debug!(task = %task.task_id, %error, "could not say where a task stands");
                continue;
            }
            self.queue_notices
                .lock()
                .expect("queue notices lock poisoned")
                .insert(
                    task.task_id.clone(),
                    QueueNotice {
                        starting: standing.start.is_some(),
                        state,
                        since,
                        at: Instant::now(),
                    },
                );
        }
        next
    }

    // What the agent says and what it is thinking both go to the session as they happen:
    // the operator watching the issue should not have to take the task over to find out
    // whether it is still working. They are different things and they are published
    // differently — what it says is part of the transcript, and what it is thinking is
    // the state it is in, replaced by the next thing this machine says. Neither is a
    // reply: nothing here is addressed to anybody and nothing here is news worth a
    // notification.
    // A task this machine has just woken says so before the agent's first line: the
    // operator who wrote in has to see that it was taken, and a session that is about
    // to work should not sit at "finished" until the model catches up.
    pub(super) async fn note_wake(&self, report: &Cursor, why: &str) {
        let id = stable_id(
            &report.task_id,
            &format!("wake-{}:{}", self.started_at_ms, now_ms()),
        );
        let body = format!("altitude is waking this task: {why}.");
        if let Err(error) = self
            .say_system(report, &id, AgentActivityContent::Thought { body })
            .await
        {
            tracing::debug!(task = %report.task_id, %error, "could not say a task was woken");
        }
    }

    pub(super) async fn report_activity(&self, host: &dyn DelegatedWork) {
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::debug!(%error, "could not read bound tasks to report activity");
                return;
            }
        };
        for task in bound {
            let activity = match host.current_activity(task.task_id.clone()).await {
                Ok(Some(activity)) if !activity.text.trim().is_empty() => activity,
                _ => continue,
            };
            let Some(report) = self.report_for_task(&task.task_id) else {
                continue;
            };
            // The count and the last line are the report's own bookkeeping rather than
            // this process's: naming an activity from a counter kept in memory meant a
            // restart reused ids Linear already had, and every publish after it was
            // accepted as a duplicate of something nobody could see.
            let (said, last_line) = self.activity_state(&task.task_id);
            if last_line.as_deref() == Some(activity.text.as_str()) {
                continue;
            }
            let published = said.saturating_add(1);
            let activity_id = stable_id(
                &report.task_id,
                &format!("activity-{}:{published}", self.started_at_ms),
            );
            let published_activity = match activity.kind {
                ActivityKind::Thinking => {
                    self.say_agent_ephemerally(&report, &activity_id, activity.text.clone())
                        .await
                }
                // What the agent says is what it has concluded, and a conclusion is
                // the one thing in a run that is addressed to a person: as a message it
                // is read where the operator already is, and in the tracker it lands in
                // the session's own thread instead of being folded into the reasoning.
                // The machine's own news is written where it does not read as a
                // message, and it already carries the mountain.
                ActivityKind::Altitude => {
                    self.say_system(
                        &report,
                        &activity_id,
                        AgentActivityContent::Thought {
                            body: activity.text.clone(),
                        },
                    )
                    .await
                }
                ActivityKind::Speech | ActivityKind::Remote => {
                    let said = self
                        .say_agent(
                            &report,
                            &activity_id,
                            AgentActivityContent::Response {
                                body: activity.text.clone(),
                            },
                        )
                        .await;
                    // A message finishes the turn it is sent in, and a run says several
                    // things before it stops: without this the session shows "finished"
                    // between the agent's own sentences. The note after it is the
                    // machine saying it has not stopped, and it is replaced by the next
                    // thing the run says either way.
                    if said.is_ok() && task.status == TaskExecutionStatus::Running {
                        self.say_system_ephemerally(
                            &report,
                            &stable_id(
                                &report.task_id,
                                &format!("working-{}:{published}", self.started_at_ms),
                            ),
                            "Working\u{2026}".to_owned(),
                        )
                        .await
                    } else {
                        said
                    }
                }
            };
            if let Err(error) = published_activity {
                tracing::debug!(task = %task.task_id, %error, "could not say what an agent is doing");
                continue;
            }
            if let Err(error) = self.record_activity(&task.task_id, published, activity.text) {
                tracing::warn!(task = %task.task_id, %error, "could not record what was said in a session");
            }
        }
    }

    // When the line started saying this, so the number on it is the operator's own
    // clock. It resets when the task moves from waiting to starting and not when the
    // machine's counts move under it: a second slot filling is not a new wait.
    pub(super) fn queue_state_since(&self, task_id: &str, standing: &QueueStanding) -> Instant {
        let notices = self
            .queue_notices
            .lock()
            .expect("queue notices lock poisoned");
        match notices.get(task_id) {
            Some(notice) if notice.starting == standing.start.is_some() => notice.since,
            _ => Instant::now(),
        }
    }

    // When a line that says what the last one said becomes worth saying again.
    fn queue_notice_due_at(&self, task_id: &str, ahead: usize, state: &str) -> Option<Instant> {
        let notices = self
            .queue_notices
            .lock()
            .expect("queue notices lock poisoned");
        let last = notices.get(task_id)?;
        if last.state != state {
            return None;
        }
        Some(last.at + Self::queue_notice_interval(ahead))
    }

    pub(super) fn queue_notice_is_due(&self, task_id: &str, ahead: usize, state: &str) -> bool {
        let notices = self
            .queue_notices
            .lock()
            .expect("queue notices lock poisoned");
        let Some(last) = notices.get(task_id) else {
            return true;
        };
        // What the machine is doing changing is news even when the queue number
        // did not move: it is what tells a wait apart from a machine that stopped.
        if last.state != state {
            return true;
        }
        // The age is the only thing the line says while nothing moves, and it is
        // worth a refresh on its own for as long as a task is the next one up.
        last.at.elapsed() >= Self::queue_notice_interval(ahead)
    }

    fn queue_notice_interval(ahead: usize) -> Duration {
        if ahead == 0 {
            NEXT_UP_NOTICE_INTERVAL
        } else {
            QUEUE_NOTICE_INTERVAL
        }
    }

    pub(super) async fn publish_signal(
        &self,
        report: &Cursor,
        signal_id: &str,
        kind: TaskAskKind,
        title: &str,
        body: &str,
        contributions: &[String],
    ) -> Result<Option<TrackerStatus>, String> {
        let marker = ask_marker(&report.task_id, signal_id);
        let body = self.hosted_body(body).await;
        let (body, status) = match kind {
            TaskAskKind::Steer => {
                // A question is not a review: the work is still in progress, and
                // moving the issue would tell a reader the milestone is finished.
                // The ask is what asks for attention.
                let who = self.operator_mention(&report.tracker_user_id).await;
                (
                    format!("**? Needs you:** {}\n\n{who} {}", title.trim(), body.trim()),
                    None,
                )
            }
            TaskAskKind::Review => {
                let who = self.operator_mention(&report.tracker_user_id).await;
                let mut body = format!(
                    "**◆ Ready to review:** {}\n\n{who} {}",
                    title.trim(),
                    body.trim()
                );
                append_contributions(&mut body, contributions);
                (body, Some(TrackerStatus::InReview))
            }
        };
        self.config
            .automation
            .open_ask(&self.issue_reference(&report.issue_id), &marker, &body)
            .await
            .map_err(|error| error.to_string())?;
        Ok(status)
    }

    // A reply is posted into the discussion it answers, idempotently: a cursor can
    // replay an event it never managed to finish, and a second copy of a sentence is
    // the one thing a conversation cannot recover from.
    pub(super) async fn post_signal_reply(&self, report: &Cursor, signal_id: &str, body: &str) {
        let marker = ask_marker(&report.task_id, signal_id);
        match self.config.automation.has_ask(&marker).await {
            Ok(true) => {}
            // A discussion this machine never opened has nowhere to put a reply.
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(task = %report.task_id, signal = signal_id, %error, "could not read a signal's discussion");
                return;
            }
        }
        let id = stable_id("reply", &format!("{}:{signal_id}:{body}", report.task_id));
        if let Err(error) = self
            .config
            .automation
            .answer_ask(&self.issue_reference(&report.issue_id), &marker, &id, body)
            .await
        {
            tracing::warn!(
                task = %report.task_id,
                signal = signal_id,
                %error,
                "could not post the agent's reply to a discussion"
            );
        }
    }

    pub(super) async fn follow_signal_thread(
        &self,
        report: &Cursor,
        signal_id: &str,
        resolved: bool,
    ) {
        let marker = ask_marker(&report.task_id, signal_id);
        // A signal resolved before this machine could report it has no discussion to
        // close: asking first is how that stays a fact rather than an error the
        // provider might also be giving for a real failure.
        match self.config.automation.has_ask(&marker).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(task = %report.task_id, signal = signal_id, %error, "could not read a signal's discussion");
                return;
            }
        }
        if let Err(error) = self.config.automation.close_ask(&marker, resolved).await {
            tracing::warn!(
                task = %report.task_id,
                signal = signal_id,
                resolved,
                %error,
                "could not follow a signal's discussion in the tracker"
            );
        }
    }

    pub(super) async fn resolve_signal_comments(
        &self,
        report: &Cursor,
        events: &[(i64, TaskHistoryEvent)],
    ) {
        let mut seen = HashSet::new();
        for (_, event) in events {
            let TaskHistoryEvent::OperatorSignalRaised { signal } = event else {
                continue;
            };
            if !seen.insert(signal.id.clone()) {
                continue;
            }
            self.follow_signal_thread(report, &signal.id, true).await;
        }
    }

    pub(super) async fn write_status(
        &self,
        host: &dyn DelegatedWork,
        report: &Cursor,
        status: TrackerStatus,
    ) -> Result<(), String> {
        let held = self.report_for_task(&report.task_id);
        if held.as_ref().and_then(|held| held.last_status) == Some(status) {
            return Ok(());
        }
        self.config
            .automation
            .status(&self.issue_reference(&report.issue_id), status)
            .await
            .map_err(|error| error.to_string())?;
        // The task's own history says what the issue says, so a card reads it
        // from the record rather than a second field that can disagree.
        if let Err(error) = host.record_status(report.task_id.clone(), status).await {
            tracing::warn!(task = %report.task_id, %error, "could not record the issue's state");
        }
        // The write happened, so a failure to remember it costs one repeat at
        // most, which is not worth failing the pass over.
        if let Err(error) = self.mutate(|ledger| {
            if let Some(held) = ledger
                .cursors
                .iter_mut()
                .find(|held| held.task_id == report.task_id)
            {
                held.last_status = Some(status);
            }
        }) {
            tracing::warn!(task = %report.task_id, %error, "could not record a status already written");
        }
        Ok(())
    }

    pub(super) fn forget_status(&self, issue_id: &str) -> Result<(), String> {
        self.mutate(|ledger| {
            for held in ledger
                .cursors
                .iter_mut()
                .filter(|held| held.issue_id == issue_id)
            {
                held.last_status = None;
            }
        })
    }

    pub(super) async fn say_task_is_running(&self, host: &dyn DelegatedWork, report: &Cursor) {
        if self.is_settled(&report.issue_id) {
            return;
        }
        if let Err(error) = self
            .write_status(host, report, TrackerStatus::InProgress)
            .await
        {
            tracing::warn!(issue = %report.issue_id, %error, "could not say the task is running again");
        }
    }

    pub(super) async fn finish_accepted(
        &self,
        host: &dyn DelegatedWork,
        report: &Cursor,
        events: &[(i64, TaskHistoryEvent)],
    ) -> Result<(), String> {
        // Accepted is finished, whichever door the acceptance came through, so the
        // threads the task was waiting in close here rather than in each caller.
        self.resolve_signal_comments(report, events).await;
        if !self.is_settled(&report.issue_id) {
            self.write_status(host, report, TrackerStatus::Done).await?;
            self.settle(&report.issue_id)?;
        }
        // The activity is what the session shows, not the issue's state: a failure
        // to say it must not hold the cursor on an ending that has been written.
        if let Err(error) = self
            .say_system(
                report,
                &stable_id(&report.task_id, "accepted"),
                AgentActivityContent::Response {
                    body: accepted_body(&contribution_urls(events)),
                },
            )
            .await
        {
            tracing::warn!(task = %report.task_id, %error, "could not post an acceptance answer");
        }
        self.finish_report(report)
    }

    // A picture in a signal becomes a picture in Linear's storage, which is the only
    // form an issue comment can show. The bytes were vetted when they entered
    // altitude, so nothing is checked again here.
    pub(super) async fn hosted_body(&self, body: &str) -> String {
        let (text, pictures) = picture::take(body);
        if pictures.is_empty() {
            return body.to_owned();
        }
        let mut hosted = text;
        for stored in pictures {
            let alt = stored.alt;
            let bytes = picture::read(&self.config.attachments_dir, &stored.id);
            let Ok(bytes) = bytes else {
                hosted.push_str(&format!("\n\n_(the picture {alt} is not on this machine)_"));
                continue;
            };
            let mime = picture::mime(&bytes).unwrap_or("image/png");
            let name = format!("{}.{}", stored.id, mime.rsplit('/').next().unwrap_or("png"));
            match self
                .config
                .automation
                .upload_picture(&bytes, mime, &name)
                .await
            {
                // Linear keeps an upload to itself: the URL is readable by whoever can
                // read the issue, and by nobody else. It matters less than it sounds,
                // because a synced issue mirror carries no comments at all — the
                // picture lives where the comment lives, and nowhere else.
                Ok(url) => hosted.push_str(&format!("\n\n![{alt}]({url})")),
                Err(error) => {
                    tracing::warn!(%error, "could not give the tracker a picture");
                    hosted.push_str(&format!("\n\n_(the picture {alt} could not be sent)_"));
                }
            }
        }
        hosted
    }

    pub(super) async fn finish_transferred(&self, report: &Cursor) -> Result<(), String> {
        self.say_system(
            report,
            &stable_id(&report.task_id, "transferred"),
            AgentActivityContent::Response {
                body: "The operator handed this task to another machine, and it keeps \
                       running there."
                    .to_owned(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        self.finish_report(report)
    }

    pub(super) async fn replay_signals(
        &self,
        host: &dyn DelegatedWork,
        report: &Cursor,
        events: &[(i64, TaskHistoryEvent)],
    ) -> Result<(), String> {
        let threads = host.signal_threads(report.task_id.clone()).await?;
        let contributions = contribution_details(events);
        for signal in threads.iter().filter(|signal| signal.is_unresolved()) {
            let Some(message) = signal.latest_agent_message() else {
                continue;
            };
            if let Some(status) = self
                .publish_signal(
                    report,
                    &signal.id,
                    signal.kind,
                    &signal.title,
                    &message.body,
                    &contributions,
                )
                .await?
            {
                self.write_status(host, report, status).await?;
            }
        }
        self.advance_cursor(&report.task_id, events.len())
    }
}

// Where a queued task stands, in one line, with how long it has stood there: the
// queue says what is in front of it, the machine says why it is not moving, and the
// age is what tells a slow wait apart from one nothing is going to end.
fn queue_notice_line(state: &str, since: Instant) -> String {
    match waited_words(since.elapsed()) {
        Some(waited) => format!("{state} · {waited}."),
        None => format!("{state}."),
    }
}

fn queue_notice_state(standing: &QueueStanding) -> String {
    // A task the machine has picked up is not waiting on the queue any more. The
    // minutes it spends starting are the ones an operator is most likely to be
    // watching, so they say what is being done rather than what is being waited for.
    match &standing.start {
        Some(Start::Provisioning) => {
            return "Starting on the remote · making its sandbox".to_owned();
        }
        Some(Start::Building(progress)) => {
            return format!(
                "Starting on the remote · {} · step {} of {}",
                progress.label,
                progress.completed + 1,
                progress.total
            );
        }
        None => {}
    }
    let place = match standing.ahead {
        0 => "Next up on the remote".to_owned(),
        1 => "Queued on the remote · 1 task ahead".to_owned(),
        ahead => format!("Queued on the remote · {ahead} tasks ahead"),
    };
    // What is in the way, when something is: the machine's own reason if it has one,
    // and otherwise the work it is already carrying.
    match standing.holding {
        Some(pressure) => format!("{place} · {}", pressure_words(pressure)),
        None => match standing.on_machine {
            0 => place,
            1 => format!("{place} · 1 task is on the machine"),
            on_machine => format!("{place} · {on_machine} tasks are on the machine"),
        },
    }
}

// The machine says what it is busy with; the operator reads why their task is waiting.
fn pressure_words(pressure: Pressure) -> &'static str {
    match pressure {
        Pressure::Cpu => "the machine is using as much cpu as it will take",
        Pressure::Memory => "the machine is using as much memory as it will take",
        Pressure::Disk => "the machine is low on disk",
    }
}

// Minutes, because that is the unit an operator waiting on work counts in, and the
// first minute is left to the next line that has something to say.
fn waited_words(elapsed: Duration) -> Option<String> {
    match elapsed.as_secs() / 60 {
        0 => None,
        minutes => Some(format!("{minutes}m")),
    }
}
