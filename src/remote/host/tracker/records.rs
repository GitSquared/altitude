use super::ledger::{Cursor, Ledger, TerminatedSession};
use crate::tracker::{IssueReference, TrackerKind};
use std::sync::MutexGuard;

use super::*;

impl RemoteTracker {
    pub(super) fn lock(&self) -> Result<MutexGuard<'_, Ledger>, String> {
        self.ledger
            .lock()
            .map_err(|_| "the tracker ledger lock was poisoned".to_owned())
    }

    pub(super) fn mutate<T>(&self, change: impl FnOnce(&mut Ledger) -> T) -> Result<T, String> {
        let mut ledger = self.lock()?;
        let mut next = (*ledger).clone();
        let outcome = change(&mut next);
        self.persist(&next)?;
        *ledger = next;
        Ok(outcome)
    }

    pub(super) fn persist(&self, ledger: &Ledger) -> Result<(), String> {
        persist(&self.config.storage_path, ledger)
    }

    pub(super) fn settle(&self, issue_id: &str) -> Result<(), String> {
        self.mutate(|ledger| {
            if !ledger.settled_issues.iter().any(|id| id == issue_id) {
                ledger.settled_issues.push(issue_id.to_owned());
            }
        })
    }

    pub(super) fn is_settled(&self, issue_id: &str) -> bool {
        self.lock()
            .map(|ledger| ledger.settled_issues.iter().any(|id| id == issue_id))
            .unwrap_or(false)
    }

    pub(super) fn report_for_task(&self, task_id: &str) -> Option<Cursor> {
        let ledger = self.lock().ok()?;
        ledger
            .cursors
            .iter()
            .find(|report| report.task_id == task_id)
            .cloned()
    }

    // The report a session is talking to. A session outlives the task that opened it,
    // so more than one report can name it: the one still in play is the task a message
    // arriving in that thread is for, and a finished one is what is left when the work
    // on the issue is over.
    pub(super) fn report_for_session(&self, session_id: &str) -> Option<Cursor> {
        let ledger = self.lock().ok()?;
        let reports: Vec<&Cursor> = ledger
            .cursors
            .iter()
            .filter(|report| report.session == session_id)
            .collect();
        reports
            .iter()
            .find(|report| !report.finished)
            .or_else(|| reports.first())
            .map(|report| (*report).clone())
    }

    // The session this issue is reported in: the one a report still in play is using,
    // and otherwise the one the issue's last report used. A session is the issue's
    // conversation — the thread the operator reads — not a task's, so a task that
    // replaces another on the same issue carries that conversation on instead of
    // starting one the issue never shows.
    pub(super) fn session_for_issue(&self, issue_id: &str) -> Option<String> {
        let ledger = self.lock().ok()?;
        let reports: Vec<&Cursor> = ledger
            .cursors
            .iter()
            .filter(|report| report.issue_id == issue_id)
            .collect();
        reports
            .iter()
            .find(|report| !report.finished)
            .or_else(|| reports.last())
            .map(|report| report.session.clone())
    }

    // A report that is over says nothing about what this machine is doing now: its
    // task was deleted, or its work ended. A delegation that arrives afterwards is
    // new work rather than a session this machine has already answered.
    pub(super) fn report_in_play_for_session(&self, session_id: &str) -> Option<Cursor> {
        self.report_for_session(session_id)
            .filter(|report| !report.finished)
    }

    pub(super) fn cursors(&self) -> Vec<Cursor> {
        self.lock()
            .map(|ledger| ledger.cursors.clone())
            .unwrap_or_default()
    }

    pub(super) fn upsert_report(&self, report: Cursor) -> Result<(), String> {
        self.mutate(|ledger| {
            ledger
                .cursors
                .retain(|existing| existing.task_id != report.task_id);
            ledger.cursors.push(report);
        })
    }

    pub(super) fn finish_report(&self, report: &Cursor) -> Result<(), String> {
        self.mutate(|ledger| {
            for existing in &mut ledger.cursors {
                if existing.task_id == report.task_id {
                    existing.finished = true;
                }
            }
        })
    }

    pub(super) fn retire_report(&self, report: &Cursor) -> Result<(), String> {
        let now = now_ms();
        self.mutate(|ledger| {
            ledger
                .cursors
                .retain(|existing| existing.task_id != report.task_id);
            ledger
                .terminated_sessions
                .retain(|session| session.session_id != report.session);
            ledger.terminated_sessions.push(TerminatedSession {
                session_id: report.session.clone(),
                terminated_at_ms: now,
            });
            ledger.prune_terminated(now);
        })
    }

    pub(super) fn is_terminated(&self, session_id: &str) -> bool {
        self.lock()
            .map(|ledger| {
                ledger
                    .terminated_sessions
                    .iter()
                    .any(|session| session.session_id == session_id)
            })
            .unwrap_or(false)
    }

    // What has been said in this task's session so far: the count its activities are
    // named by, and the last line, which is what a pass compares against.
    pub(super) fn activity_state(&self, task_id: &str) -> (u32, Option<String>) {
        self.lock()
            .ok()
            .and_then(|ledger| {
                ledger
                    .cursors
                    .iter()
                    .find(|report| report.task_id == task_id)
                    .map(|report| (report.activities, report.activity_line.clone()))
            })
            .unwrap_or((0, None))
    }

    pub(super) fn record_activity(
        &self,
        task_id: &str,
        published: u32,
        line: String,
    ) -> Result<(), String> {
        self.mutate(|ledger| {
            if let Some(report) = ledger.cursors.iter_mut().find(|r| r.task_id == task_id) {
                report.activities = published;
                report.activity_line = Some(line);
            }
        })
    }

    pub(super) fn advance_cursor(&self, task_id: &str, cursor: usize) -> Result<(), String> {
        self.mutate(|ledger| {
            if let Some(report) = ledger.cursors.iter_mut().find(|r| r.task_id == task_id) {
                report.reported_events = cursor;
            }
        })
    }

    pub(super) fn issue_reference(&self, issue_id: &str) -> IssueReference {
        IssueReference {
            provider: TrackerKind::Linear,
            workspace_id: self.config.automation.workspace_id().to_owned(),
            id: issue_id.to_owned(),
            identifier: String::new(),
            url: String::new(),
        }
    }

    pub(super) fn owner_for_user(&self, tracker_user_id: &str) -> Option<String> {
        logins(&self.config.operators_dir)
            .into_iter()
            .filter(|login| login.account.user_id == tracker_user_id)
            .max_by(|left, right| {
                left.arrived
                    .cmp(&right.arrived)
                    .then_with(|| right.owner.cmp(&left.owner))
            })
            .map(|login| login.owner)
    }

    pub(super) fn user_for_owner(&self, owner: &str) -> Option<String> {
        logins(&self.config.operators_dir)
            .into_iter()
            .find(|login| login.owner == owner)
            .map(|login| login.account.user_id)
    }
}
