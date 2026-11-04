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

    pub(super) fn report_for_session(&self, session_id: &str) -> Option<Cursor> {
        let ledger = self.lock().ok()?;
        ledger
            .cursors
            .iter()
            .find(|report| report.session == session_id)
            .cloned()
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
