use super::ledger::{Cursor, SessionEvent, TerminatedSession};
use crate::{
    task::{TaskAsk, TaskAskKind, TaskExecutionPauseReason, TaskExecutionStatus, TaskHistoryEvent},
    tracker::providers::linear::automation::AgentActivityContent,
};
use iroh::EndpointId;

use crate::tracker::{Login, logins};

use super::*;

mod mentions;
mod sessions;

impl RemoteTracker {
    pub(super) async fn recover_delegations(&self, host: &dyn DelegatedWork) {
        let delegated = match self.config.automation.delegated_issues().await {
            Ok(delegated) => delegated,
            Err(error) => {
                tracing::debug!(%error, "could not ask which issues are delegated to this app");
                return;
            }
        };
        for delegated in delegated {
            let reference = &delegated.reference;
            // This machine has already worked this delegation: the session it was
            // asked in has a cursor, so the work happened, and a task that ended is
            // not a task to start again. Without this the sweep adopts the same
            // finished delegation every minute it runs, forever.
            if self.is_settled(&reference.id)
                || delegated
                    .session
                    .iter()
                    .any(|session| self.report_for_session(session).is_some())
            {
                continue;
            }
            // Work already in play on the issue means this delegation was delivered and
            // answered: a prompt that arrived while the machine was up was steered into
            // the task holding the issue, and nothing was left for a scan to find. Adopting
            // it now would stop that task and replace a conversation the operator is
            // having with a second one, which is what a boot did to a delegation that had
            // been redirected minutes before.
            match host.work_for_issue(reference.id.clone(), Vec::new()).await {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(issue = %reference.identifier, %error, "could not ask which task an issue has");
                    continue;
                }
            }
            // A delegation is news while it is new. One nobody picked up when it was
            // made is not quietly started days later because a machine restarted.
            if delegated
                .delegated_at_ms
                .is_some_and(|made| now_ms() - made > STALE_DELEGATION_MS)
            {
                tracing::info!(
                    issue = %reference.identifier,
                    "a delegation older than the sweep's patience is left alone"
                );
                continue;
            }
            // Which work this issue has is a question the tasks answer. This
            // delegation's own work means nothing to do; work for another one means
            // the operator delegated again, which replaces it.
            let mine: Vec<String> = delegated.session.iter().cloned().collect();
            let own_work = match host.work_for_issue(reference.id.clone(), mine).await {
                Ok(work) => work,
                Err(error) => {
                    tracing::debug!(issue = %reference.identifier, %error, "could not ask which task an issue has");
                    continue;
                }
            };
            if own_work.is_some() {
                continue;
            }
            let Some(login) = self.sole_login() else {
                tracing::info!(
                    issue = %reference.identifier,
                    "an issue is delegated to this app and this machine holds several logins, \
                     so it cannot tell whose work it is"
                );
                continue;
            };
            // The session the operator delegated into is where they are watching,
            // so answering in it is answering where they asked. One is opened only
            // when the tracker made none.
            // A session a live task is reporting into belongs to that task, and a
            // finished task's session is not a thread to answer in: the delegation
            // made a new one, or this machine makes it.
            let adopted = delegated
                .session
                .clone()
                .filter(|session| self.report_in_play_for_session(session).is_none());
            let session = match &adopted {
                Some(session) => session.clone(),
                None => match self
                    .config
                    .automation
                    .create_session_on_issue(&reference.id)
                    .await
                {
                    Ok(session) => session,
                    Err(error) => {
                        tracing::warn!(issue = %reference.identifier, %error, "could not open a session for a delegation");
                        continue;
                    }
                },
            };
            tracing::info!(
                issue = %reference.identifier,
                "picked up a delegation the webhook did not bring"
            );
            let started = self
                .delegate(
                    &reference.id,
                    &session,
                    &login.account.user_id,
                    &login.account.display_name,
                    host,
                )
                .await;
            let Err(error) = started else {
                continue;
            };
            // A session the operator delegated into can already be closed, and a
            // closed session accepts nothing: one this machine opens is the second
            // chance rather than losing the delegation to a dead thread.
            if adopted.is_some()
                && let Ok(fresh) = self
                    .config
                    .automation
                    .create_session_on_issue(&reference.id)
                    .await
                && self
                    .delegate(
                        &reference.id,
                        &fresh,
                        &login.account.user_id,
                        &login.account.display_name,
                        host,
                    )
                    .await
                    .is_ok()
            {
                continue;
            }
            tracing::warn!(issue = %reference.identifier, %error, "could not start a recovered delegation");
        }
    }

    pub(super) async fn adopt_bound_tasks(&self, host: &dyn DelegatedWork) {
        let bound = match host.bound_tasks().await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::warn!(%error, "could not read the tasks bound to an issue");
                return;
            }
        };
        for task in bound {
            // A refusal that waits for nothing is not asked again in this process: an
            // issue the tracker no longer has does not come back by asking every ten
            // seconds, and a restart asks once.
            let refused_for_good = self
                .refusals
                .lock()
                .ok()
                .and_then(|refusals| refusals.get(&task.task_id).cloned())
                .is_some_and(|reason| is_absence(&reason));
            if refused_for_good {
                continue;
            }
            // A task this machine is already telling the issue about needs nothing
            // here; whether the issue has work at all is the question the tasks
            // answer, and adopting a task the tracker did not just ask for is what
            // attached new work to an old one. Every task in this list is still in
            // play, because that is what `bound_tasks` answers.
            //
            // A report that *ended* is not that: unhooking ends the report, and the
            // operator's next message starts the task again, so the work goes on with
            // nobody telling the issue about it and the questions it asks are asked
            // into a thread that was closed.
            if self
                .report_for_task(&task.task_id)
                .is_some_and(|report| !report.finished)
                || task.delegation.is_none()
                || self.user_for_owner(&task.owner).is_none()
            {
                continue;
            }
            let outcome = self
                .start_reporting(
                    &task.owner,
                    task.task_id.clone(),
                    task.issue.clone(),
                    task.delegation.clone(),
                    task.session_agent.clone(),
                )
                .await;
            let refusal = outcome.err();
            let changed = self
                .refusals
                .lock()
                .map(|mut refusals| match &refusal {
                    Some(reason) => {
                        refusals.insert(task.task_id.clone(), reason.clone())
                            != Some(reason.clone())
                    }
                    None => refusals.remove(&task.task_id).is_some(),
                })
                .unwrap_or(false);
            match refusal {
                Some(reason) if changed => tracing::warn!(
                    task = %task.task_id,
                    %reason,
                    "could not start reporting on a bound task"
                ),
                _ => {}
            }
        }
    }

    pub(super) async fn delegate_new_session(
        &self,
        event: &SessionEvent,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        let Some(issue_id) = event.issue_id.as_deref() else {
            return Ok(());
        };
        if self.is_terminated(&event.session_id) {
            return Ok(());
        }
        // The session's creator is the delegation author. A session this app opens is
        // one it opened to report into, and a session another app opens has an app to
        // answer for it: neither has a person to report through, and saying anything
        // about the machine's setup in them is advice nobody asked this machine for.
        // That advice is for a delegation a person made, and it is given further down.
        let Some(author) = event
            .creator_id
            .as_deref()
            .filter(|id| *id != self.config.automation.app_user_id())
        else {
            return Ok(());
        };
        let label = event.creator_name.as_deref().unwrap_or(author);
        if self
            .mention_beside_task(event, Some(author), event.prompt_body.as_deref(), host)
            .await?
        {
            return Ok(());
        }
        self.delegate(issue_id, &event.session_id, author, label, host)
            .await
    }

    // A mention opens a session beside the task that holds the issue: a message into
    // that task, never a redelegation. The operator asking the agent something in a
    // new thread has not asked for the conversation to start over, so nothing here
    // starts work — it says the message where the task can read it.
    //
    pub(super) async fn delegate(
        &self,
        issue_id: &str,
        session_id: &str,
        author: &str,
        label: &str,
        host: &dyn DelegatedWork,
    ) -> Result<(), String> {
        if self.is_terminated(session_id) {
            return Ok(());
        }
        // The same session delivered again is the work already running here; it
        // must not be stopped and restarted. A task that failed is not running: it
        // stopped before it could work on the issue, so delegating again is the only
        // way to ask for it, and treating it as work in progress would leave an issue
        // nobody could delegate to twice.
        if let Some(work) = host
            .work_for_issue(issue_id.to_owned(), vec![session_id.to_owned()])
            .await?
            && work.status != TaskExecutionStatus::Failed
        {
            tracing::info!(
                issue = %issue_id,
                session = %session_id,
                task = %work.task_id,
                "a delegation arrived for work already on this machine"
            );
            return Ok(());
        }

        // A different session on an issue that already has work is a
        // redelegation: every task still bound to the issue stops before the new
        // one starts, so two capsules never own the same issue — and so the work
        // that starts is the new delegation rather than one an older task quietly
        // inherited, because a task leaves the issue to whoever founds it next only
        // once it is out of play.
        let replaced: Vec<BoundTask> = host
            .work_for_issue(issue_id.to_owned(), Vec::new())
            .await?
            .into_iter()
            .collect();
        let redelegation = !replaced.is_empty();
        for task in &replaced {
            if let Err(error) = host
                .stop(
                    task.task_id.clone(),
                    TaskExecutionPauseReason::DelegationWithdrawn,
                )
                .await
            {
                tracing::warn!(task = %task.task_id, %error, "could not stop a replaced task");
                continue;
            }
            if let Some(report) = self.report_for_task(&task.task_id) {
                let body = "A newer delegation replaced this one, so altitude stopped this \
                            task.";
                if let Err(error) = self
                    .say_system(
                        &report,
                        &stable_id(&task.task_id, "redelegated"),
                        AgentActivityContent::Response {
                            body: body.to_owned(),
                        },
                    )
                    .await
                {
                    tracing::warn!(task = %task.task_id, %error, "could not say a task was replaced");
                }
                if let Err(error) = self.retire_report(&report) {
                    tracing::warn!(task = %task.task_id, %error, "could not retire a replaced task's report");
                }
            }
        }

        let Some(owner) = self.owner_for_user(author) else {
            self.post_sync_guidance(session_id).await?;
            return Ok(());
        };
        // Work the tracker is not asking for is work nothing will keep watching: the
        // pass that reads the issue's delegation stops a task the issue does not name
        // this app for. An operator who mentions the app on an issue nobody delegated
        // is asking for it, and the tracker is where that is written down — one
        // minute later the task they asked for was being killed for the delegation
        // nobody had made.
        let delegation = self
            .config
            .automation
            .issue_delegation(issue_id)
            .await
            .map_err(|error| error.to_string())?;
        if delegation.delegate_id.as_deref() != Some(self.config.automation.app_user_id()) {
            if delegation.delegate_id.is_some() {
                tracing::info!(issue = %issue_id, "an issue delegated to another app is not this machine's work");
                return Ok(());
            }
            if let Err(error) = self
                .config
                .automation
                .claim_issue(&self.issue_reference(issue_id))
                .await
            {
                tracing::warn!(issue = %issue_id, %error, "could not delegate an issue asked for by a mention");
                return Ok(());
            }
            tracing::info!(
                issue = %issue_id,
                session = %session_id,
                "an issue asked for by a mention was delegated to this app"
            );
        }
        let peer: EndpointId = owner
            .parse()
            .map_err(|_| "the remote's key could not be read".to_owned())?;

        // Say the work is starting before the issue is fetched and a capsule is
        // provisioned. The id is stable, so a redelivery is one thought.
        self.create_system_activity(
            session_id,
            &stable_id(session_id, "starting"),
            AgentActivityContent::Thought {
                body: format!("altitude is setting up this task for {label}."),
            },
        )
        .await
        .map_err(|error| error.to_string())?;

        let snapshot = self
            .config
            .automation
            .read(&self.issue_reference(issue_id))
            .await
            .map_err(|error| error.to_string())?;
        let task_id = match host
            .start_issue(
                peer,
                snapshot,
                label.to_owned(),
                Some(session_id.to_owned()),
            )
            .await
        {
            Ok(task_id) => task_id,
            Err(refusal) => {
                // A remote that cannot start the work needs its operator to push
                // a setup. Retrying forever would wedge every later delivery.
                self.post_start_guidance(session_id, &refusal).await?;
                return Ok(());
            }
        };
        self.upsert_report(Cursor {
            issue_id: issue_id.to_owned(),
            issue_identifier: self.issue_reference(issue_id).identifier.clone(),
            session: session_id.to_owned(),
            owner,
            tracker_user_id: author.to_owned(),
            task_id: task_id.clone(),
            created_at_ms: now_ms(),
            reported_events: 0,
            finished: false,
            last_status: None,
            failed_at: None,
            failed_attempts: 0,
            unhooked_at: None,
            activities: 0,
            activity_line: None,
        })?;
        // New work on an issue is a new cycle: whatever ending was written for the
        // last task there is not this one's.
        self.mutate(|ledger| ledger.settled_issues.retain(|id| id != issue_id))?;
        let session_agent = host.session_agent(task_id.clone()).await?;
        if let Err(error) = self
            .summarise_session(session_id, author, session_agent.as_deref())
            .await
        {
            tracing::warn!(session = %session_id, %error, "could not name a delegated session");
        }
        if redelegation {
            self.create_system_activity(
                session_id,
                &stable_id(&task_id, "redelegated"),
                AgentActivityContent::Thought {
                    body: format!("Redelegated: this task now runs for {label}."),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn sole_login(&self) -> Option<Login> {
        let mut logins = logins(&self.config.operators_dir);
        if logins.len() != 1 {
            return None;
        }
        logins.pop()
    }
}
