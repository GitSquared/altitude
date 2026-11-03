use super::ledger::Cursor;
use crate::{
    task::{TaskHistoryEvent, TaskOperatorSignalKind, picture},
    tracker::{IssueReference, TrackerStatus, providers::linear::automation::AgentActivityContent},
};
use std::{collections::HashSet, time::Instant};

use super::*;
pub(super) struct QueueNotice {
    ahead: usize,
    at: Instant,
}

// The last line published for a task, and how many before it: the count is what
// keeps two identical lines apart when the agent says the same thing twice.
pub(super) struct ActivityNotice {
    line: String,
    published: usize,
}

impl RemoteTracker {
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
        // Work a delegation asked for is told to that delegation: opening a session
        // beside it is how one issue ended up with two threads about one task.
        let session_id = match delegation {
            Some(delegation) => delegation,
            None => self
                .config
                .automation
                .create_session_on_issue(&issue.id)
                .await
                .map_err(|error| error.to_string())?,
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
            .config
            .automation
            .create_activity(
                &report.session,
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

    pub(super) async fn report_queue_standing(&self, host: &dyn Host) {
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::debug!(%error, "could not read bound tasks to report queue standing");
                return;
            }
        };
        for task in bound {
            let ahead = match host.tasks_ahead_in_queue(task.task_id.clone()).await {
                Ok(ahead) => ahead,
                Err(error) => {
                    tracing::debug!(task = %task.task_id, %error, "could not read a task's queue standing");
                    continue;
                }
            };
            let Some(report) = self.report_for_task(&task.task_id) else {
                continue;
            };
            let Some(ahead) = ahead else {
                self.queue_notices
                    .lock()
                    .expect("queue notices lock poisoned")
                    .remove(&task.task_id);
                continue;
            };
            if !self.queue_notice_is_due(&task.task_id, ahead) {
                continue;
            }
            let body = if ahead == 0 {
                "Next up on the remote · waiting for a sandbox to free.".to_owned()
            } else if ahead == 1 {
                "Queued on the remote · 1 task ahead.".to_owned()
            } else {
                format!("Queued on the remote · {ahead} tasks ahead.")
            };
            let stamp = now_ms();
            let activity_id = stable_id(&report.session, &format!("queue-{stamp}-{ahead}"));
            if let Err(error) = self
                .config
                .automation
                .create_ephemeral_thought(&report.session, &activity_id, body.clone())
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
                        ahead,
                        at: Instant::now(),
                    },
                );
        }
    }

    // What the agent is saying and thinking goes to the session as it happens: the
    // operator watching the issue should not have to take the task over to find out
    // whether it is still working. A thought rather than a reply, because nothing
    // here is addressed to anybody and nothing here is news worth a notification.
    pub(super) async fn report_activity(&self, host: &dyn Host) {
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::debug!(%error, "could not read bound tasks to report activity");
                return;
            }
        };
        for task in bound {
            let line = match host.current_activity(task.task_id.clone()).await {
                Ok(Some(line)) if !line.trim().is_empty() => line,
                _ => continue,
            };
            let Some(report) = self.report_for_task(&task.task_id) else {
                continue;
            };
            let published = {
                let notices = self
                    .activity_notices
                    .lock()
                    .expect("activity notices lock poisoned");
                match notices.get(&task.task_id) {
                    Some(notice) if notice.line == line => continue,
                    Some(notice) => notice.published.saturating_add(1),
                    None => 1,
                }
            };
            let activity_id = stable_id(&report.task_id, &format!("activity-{published}"));
            if let Err(error) = self
                .config
                .automation
                .create_activity(
                    &report.session,
                    &activity_id,
                    AgentActivityContent::Thought { body: line.clone() },
                )
                .await
            {
                tracing::debug!(task = %task.task_id, %error, "could not say what an agent is doing");
                continue;
            }
            self.activity_notices
                .lock()
                .expect("activity notices lock poisoned")
                .insert(task.task_id.clone(), ActivityNotice { line, published });
        }
    }

    pub(super) fn queue_notice_is_due(&self, task_id: &str, ahead: usize) -> bool {
        let notices = self
            .queue_notices
            .lock()
            .expect("queue notices lock poisoned");
        let Some(last) = notices.get(task_id) else {
            return true;
        };
        let interval = if ahead == 0 {
            NEXT_UP_NOTICE_INTERVAL
        } else {
            QUEUE_NOTICE_INTERVAL
        };
        last.ahead != ahead || last.at.elapsed() >= interval
    }

    pub(super) async fn publish_signal(
        &self,
        report: &Cursor,
        signal_id: &str,
        kind: TaskOperatorSignalKind,
        title: &str,
        body: &str,
        contributions: &[String],
    ) -> Result<Option<TrackerStatus>, String> {
        let comment_id = published_comment_id(&report.task_id, signal_id);
        let body = &self.hosted_body(body).await;
        match kind {
            // Updates are the session's transcript, not an issue comment.
            TaskOperatorSignalKind::Update => {
                // A reply rather than a thought: a thought is folded away in the
                // session's sidebar, and an update the operator never unfolds is an
                // update that was never delivered.
                let body = format!("**○ Info:** {}\n\n{}", title.trim(), body.trim());
                self.config
                    .automation
                    .create_activity(
                        &report.session,
                        &stable_id(&report.task_id, signal_id),
                        AgentActivityContent::Response { body },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(None)
            }
            TaskOperatorSignalKind::Decision => {
                let body = format!("**→ Decision:** {}\n\n{}", title.trim(), body.trim());
                self.config
                    .automation
                    .comment_on_issue(&report.issue_id, &body, &comment_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(None)
            }
            TaskOperatorSignalKind::Blocked => {
                // A question is not a review: the work is still in progress, and
                // moving the issue would tell a reader the milestone is finished.
                // The comment is what asks for attention.
                let who = self.operator_mention(&report.tracker_user_id).await;
                let body = format!("**? Needs you:** {}\n\n{who} {}", title.trim(), body.trim());
                self.config
                    .automation
                    .comment_on_issue(&report.issue_id, &body, &comment_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(None)
            }
            TaskOperatorSignalKind::InReview => {
                let who = self.operator_mention(&report.tracker_user_id).await;
                let mut body = format!(
                    "**◆ Ready to review:** {}\n\n{who} {}",
                    title.trim(),
                    body.trim()
                );
                append_contributions(&mut body, contributions);
                self.config
                    .automation
                    .comment_on_issue(&report.issue_id, &body, &comment_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(Some(TrackerStatus::InReview))
            }
        }
    }

    pub(super) async fn follow_signal_thread(
        &self,
        report: &Cursor,
        signal_id: &str,
        events: &[(i64, TaskHistoryEvent)],
        resolved: bool,
    ) {
        let Some(kind) = signal_kind(events, signal_id) else {
            return;
        };
        if kind == TaskOperatorSignalKind::Update {
            return;
        }
        let comment_id = published_comment_id(&report.task_id, signal_id);
        // A signal resolved before this machine could report it has no thread to
        // close: asking first is how that stays a fact rather than an error the
        // provider might also be giving for a real failure.
        match self.config.automation.comment(&comment_id).await {
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(task = %report.task_id, signal = signal_id, %error, "could not read a signal's thread");
                return;
            }
        }
        let outcome = if resolved {
            self.config
                .automation
                .resolve_comment(&comment_id, None)
                .await
        } else {
            self.config.automation.reopen_comment(&comment_id).await
        };
        if let Err(error) = outcome {
            tracing::warn!(
                task = %report.task_id,
                signal = signal_id,
                resolved,
                %error,
                "could not follow a signal's thread in the tracker"
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
            if signal.kind == TaskOperatorSignalKind::Update || !seen.insert(signal.id.clone()) {
                continue;
            }
            self.follow_signal_thread(report, &signal.id, events, true)
                .await;
        }
    }

    pub(super) async fn write_status(
        &self,
        host: &dyn Host,
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

    pub(super) async fn say_task_is_running(&self, host: &dyn Host, report: &Cursor) {
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
        host: &dyn Host,
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
            .config
            .automation
            .create_activity(
                &report.session,
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
    // Altitude, so nothing is checked again here.
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
        self.config
            .automation
            .create_activity(
                &report.session,
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
        host: &dyn Host,
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
