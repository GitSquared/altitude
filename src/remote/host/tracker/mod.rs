use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::remote::StartFuture;
use crate::remote::StartRefusal;
mod ledger;
mod logins;
mod webhook;

use ledger::{
    CommentInput, Cursor, Delivery, IssueStateChanged, Ledger, SessionEvent, StoredEvent,
    TerminatedSession,
};
pub use logins::{Login, claim_account, logins, logins_of, yield_account};
use webhook::{parse_app_notification, parse_issue_event};

// How old a delegation may be and still be picked up by a sweep. Two days is long
// enough to cover a machine that was off for a weekend and short enough that an
// issue delegated weeks ago stays a record rather than becoming work again.
const STALE_DELEGATION_MS: i64 = 48 * 60 * 60 * 1000;

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use iroh::EndpointId;
use serde::Deserialize;
use tokio::{
    sync::Notify,
    time::{MissedTickBehavior, interval},
};

use crate::{
    task::{
        TaskExecutionPauseReason, TaskHistoryEvent, TaskOperatorSignalAuthor,
        TaskOperatorSignalKind, TaskOperatorSignalThread,
    },
    tracker::{
        IssueReference, IssueSnapshot, TrackerKind, TrackerStatus,
        automation::{
            AgentActivityContent, AgentSessionEvent, CommentSite, LinearAutomation, WebhookError,
            parse_agent_session_event, verify_signature,
        },
    },
};

use super::admission::{self, Actor, Admission, InputFacts};
use crate::remote::identity::write_private;

const WEBHOOK_ROUTE: &str = "/altitude/linear/webhook";
const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;
const SIGNATURE_HEADER: &str = "linear-signature";

const TICK: Duration = Duration::from_secs(10);

const QUEUE_NOTICE_INTERVAL: Duration = Duration::from_secs(300);
const NEXT_UP_NOTICE_INTERVAL: Duration = Duration::from_secs(30);

struct QueueNotice {
    ahead: usize,
    at: Instant,
}
const DELEGATION_SWEEP_TICKS: u32 = 6;
const EVENT_ATTEMPTS: usize = 3;
const UNHOOK_GRACE_MS: i64 = 60_000;
const RETRY: Duration = Duration::from_secs(10);
const DONE_RETENTION: Duration = Duration::from_secs(600);
const TERMINATED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DONE: usize = 512;
const MAX_TERMINATED: usize = 1024;
const WEBHOOK_MAX_AGE: Duration = Duration::from_secs(300);

pub struct RemoteTrackerConfig {
    pub automation: Arc<LinearAutomation>,
    pub webhook_secret: Vec<u8>,
    pub oauth_client_id: String,
    pub storage_path: PathBuf,
    pub operators_dir: PathBuf,
    pub host_label: String,
}

pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub trait Host: Send + Sync + 'static {
    fn start_issue(
        &self,
        peer: EndpointId,
        issue: IssueSnapshot,
        label: String,
        delegation: Option<String>,
    ) -> StartFuture<'_, String>;
    fn stop(&self, task_id: String, reason: TaskExecutionPauseReason) -> HostFuture<'_, ()>;
    fn complete(&self, task_id: String) -> HostFuture<'_, ()>;
    fn record_status(&self, task_id: String, status: TrackerStatus) -> HostFuture<'_, ()>;
    fn signal_threads(&self, task_id: String) -> HostFuture<'_, Vec<TaskOperatorSignalThread>>;
    fn reopen_signal(&self, task_id: String, signal_id: String) -> HostFuture<'_, ()>;
    fn events(&self, task_id: String) -> HostFuture<'_, Vec<(i64, TaskHistoryEvent)>>;
    fn instruct(&self, task_id: String, instruction: String) -> HostFuture<'_, ()>;
    fn reply(&self, task_id: String, signal_id: String, answer: String) -> HostFuture<'_, ()>;
    fn accept(&self, task_id: String, signal_id: String) -> HostFuture<'_, ()>;
    fn is_kept(&self, task_id: String) -> HostFuture<'_, bool>;

    fn bound_tasks(&self) -> HostFuture<'_, Vec<BoundTask>>;
    fn delegation_of(&self, task_id: String) -> HostFuture<'_, Option<String>>;
    fn work_for_issue(
        &self,
        issue_id: String,
        delegations: Vec<String>,
    ) -> HostFuture<'_, Option<BoundTask>>;
    fn session_agent(&self, task_id: String) -> HostFuture<'_, Option<String>>;

    fn tasks_ahead_in_queue(&self, task_id: String) -> HostFuture<'_, Option<usize>>;
}

fn published_comment_id(task_id: &str, signal_id: &str) -> String {
    stable_id("comment", &format!("signal:{task_id}:{signal_id}"))
}

fn pause_notice(reason: &TaskExecutionPauseReason) -> &'static str {
    match reason {
        TaskExecutionPauseReason::AwaitingOperator => "Waiting for your reply on this task.",
        TaskExecutionPauseReason::AwaitingReviewChecks => {
            "Waiting for the pull request checks to settle."
        }
        TaskExecutionPauseReason::AgentWentQuiet => {
            "The agent kept taking turns without anything to report, so the task stopped \
             here. Tell it what to do next."
        }
        TaskExecutionPauseReason::ApplicationShutdown => {
            "The machine running this task restarted. The task resumes when it is back."
        }
        TaskExecutionPauseReason::Transfer => {
            "The operator is taking this task over on their own machine; it keeps running there."
        }
        TaskExecutionPauseReason::TrackerEnded => {
            "The issue this task was working on ended, so the task stopped."
        }
        TaskExecutionPauseReason::StoppedInTracker => {
            "This task was stopped in the tracker, so it stopped here."
        }
        TaskExecutionPauseReason::DelegationWithdrawn => {
            "This issue is no longer delegated to Altitude, so the task stopped."
        }
    }
}

fn worth_retrying(refusal: &str) -> bool {
    !refusal.contains("not found") && !refusal.contains("Entity not found")
}

pub struct BoundTask {
    pub task_id: String,
    pub owner: String,
    pub issue: IssueReference,
    pub delegation: Option<String>,
    pub session_agent: Option<String>,
}

pub struct RemoteTracker {
    config: RemoteTrackerConfig,
    queue_notices: Mutex<HashMap<String, QueueNotice>>,
    ledger: Mutex<Ledger>,
    notify: Notify,
    refusals: Mutex<HashMap<String, String>>,
    delivered: AtomicBool,
}

impl RemoteTracker {
    pub fn new(config: RemoteTrackerConfig) -> Result<Arc<Self>, String> {
        let ledger = Ledger::load(&config.storage_path)?;
        Ok(Arc::new(Self {
            config,
            queue_notices: Mutex::new(HashMap::new()),
            ledger: Mutex::new(ledger),
            refusals: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            delivered: AtomicBool::new(false),
        }))
    }

    pub fn webhook_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(WEBHOOK_ROUTE, post(webhook))
            .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BYTES))
            .with_state(Arc::clone(self))
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

    async fn report_queue_standing(&self, host: &dyn Host) {
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

    fn queue_notice_is_due(&self, task_id: &str, ahead: usize) -> bool {
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

    pub async fn run(self: Arc<Self>, host: Arc<dyn Host>) {
        let mut tick = interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut passes = 0u32;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = self.notify.notified() => {}
            }
            self.process_deliveries(host.as_ref()).await;
            self.adopt_bound_tasks(host.as_ref()).await;
            self.report_queue_standing(host.as_ref()).await;
            self.poll_reports(host.as_ref()).await;
            // A delegation is delivered by webhook, and a webhook that arrives
            // while this machine is restarting is gone. Asking the tracker which
            // issues are delegated is how the machine finds the work it was asked
            // for while it was not listening.
            passes += 1;
            if passes.is_multiple_of(DELEGATION_SWEEP_TICKS) {
                self.recover_delegations(host.as_ref()).await;
            }
        }
    }

    async fn recover_delegations(&self, host: &dyn Host) {
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
            // A session this machine has already reported into belongs to the task
            // that used it, and a finished task's session is not a thread to answer
            // in: the delegation made a new one, or this machine makes it.
            let adopted = delegated
                .session
                .clone()
                .filter(|session| self.report_for_session(session).is_none());
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

    fn sole_login(&self) -> Option<Login> {
        let mut logins = logins(&self.config.operators_dir);
        if logins.len() != 1 {
            return None;
        }
        logins.pop()
    }

    async fn adopt_bound_tasks(&self, host: &dyn Host) {
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
                .is_some_and(|reason| !worth_retrying(&reason));
            if refused_for_good {
                continue;
            }
            // A task with a cursor is already being told about; whether the issue
            // has work at all is the question the tasks answer, and adopting a task
            // the tracker did not just ask for is what attached new work to an old
            // one.
            if self.report_for_task(&task.task_id).is_some()
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

    fn lock(&self) -> Result<MutexGuard<'_, Ledger>, String> {
        self.ledger
            .lock()
            .map_err(|_| "the tracker ledger lock was poisoned".to_owned())
    }

    fn mutate<T>(&self, change: impl FnOnce(&mut Ledger) -> T) -> Result<T, String> {
        let mut ledger = self.lock()?;
        let mut next = (*ledger).clone();
        let outcome = change(&mut next);
        self.persist(&next)?;
        *ledger = next;
        Ok(outcome)
    }

    fn persist(&self, ledger: &Ledger) -> Result<(), String> {
        persist(&self.config.storage_path, ledger)
    }

    fn enqueue(&self, key: String, event: StoredEvent) -> Result<bool, String> {
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

    fn pending(&self) -> Vec<Delivery> {
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

    fn complete_delivery(&self, key: &str) -> Result<(), String> {
        self.mutate(|ledger| {
            if let Some(delivery) = ledger.deliveries.iter_mut().find(|d| d.key == key) {
                delivery.done = true;
            }
            ledger.prune(now_ms());
        })
    }

    fn defer_delivery(&self, key: &str) -> Result<(), String> {
        let retry_at = now_ms() + RETRY.as_millis() as i64;
        self.mutate(|ledger| {
            if let Some(delivery) = ledger.deliveries.iter_mut().find(|d| d.key == key) {
                delivery.next_attempt_at_ms = retry_at;
            }
        })
    }

    async fn process_deliveries(&self, host: &dyn Host) {
        for delivery in self.pending() {
            if delivery.next_attempt_at_ms > now_ms() {
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
                }
            }
        }
    }

    async fn apply_delivery(&self, event: &StoredEvent, host: &dyn Host) -> Result<(), String> {
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
        }
    }

    async fn reopen_closed_thread(&self, report: &Cursor, site: &CommentSite, host: &dyn Host) {
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

    async fn stop_for_terminated_session(
        &self,
        event: &SessionEvent,
        host: &dyn Host,
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
        self.config
            .automation
            .create_activity(
                &report.session,
                &stable_id(&report.task_id, "stopped"),
                AgentActivityContent::Response {
                    body: "Stopped. Altitude stopped this task and handed the issue back; \
                           the work it had done is still in Altitude, if you want it."
                        .to_owned(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        self.finish_report(&report)
    }

    async fn delegate_new_session(
        &self,
        event: &SessionEvent,
        host: &dyn Host,
    ) -> Result<(), String> {
        let Some(issue_id) = event.issue_id.as_deref() else {
            return Ok(());
        };
        if self.is_terminated(&event.session_id)
            || self.report_for_session(&event.session_id).is_some()
        {
            return Ok(());
        }
        // The session's creator is the delegation author. An automation- or
        // agent-started session has no responsible human, so there is nobody to
        // report through and no assignee fallback is invented. An app-created id
        // cannot be guessed back to a person, and an issue already being worked
        // on is not news, so only an unknown issue is worth educating.
        let Some(author) = event
            .creator_id
            .as_deref()
            .filter(|id| *id != self.config.automation.app_user_id())
        else {
            if host
                .work_for_issue(issue_id.to_owned(), Vec::new())
                .await?
                .is_none()
            {
                self.post_sync_guidance(&event.session_id, Unprepared::NoProjects)
                    .await?;
            }
            return Ok(());
        };
        let label = event.creator_name.as_deref().unwrap_or(author);
        // A mention creates a new session beside the existing task. It is a
        // message into that task, never a redelegation. A delegation's comment is
        // the generated session root; a mention's comment is a human thread.
        let is_mention = match event.comment_id.as_deref() {
            Some(comment_id) => {
                !self
                    .config
                    .automation
                    .comment_site(comment_id)
                    .await
                    .map_err(|error| error.to_string())?
                    .is_artificial_session_root
            }
            None => false,
        };
        // A teammate's mention is a boundary case: acknowledge it and name the
        // operator, but do not run work for them.
        let working = host
            .work_for_issue(issue_id.to_owned(), Vec::new())
            .await?
            .and_then(|task| self.report_for_task(&task.task_id));
        if let Some(report) = working
            && is_mention
        {
            if let Some(source_comment_id) = event
                .source_comment_id
                .as_ref()
                .or(event.comment_id.as_ref())
            {
                self.react_eyes(source_comment_id).await;
            }
            if author == report.tracker_user_id {
                let body = event.prompt_body.clone().unwrap_or_default();
                if !body.trim().is_empty() {
                    host.instruct(
                        report.task_id,
                        format!(
                            "A mention from the task's operator:\n\n{body}\n\n\
                             This comment was addressed to you. Continue the task with it."
                        ),
                    )
                    .await?;
                    self.config
                        .automation
                        .create_activity(
                            &event.session_id,
                            &stable_id(&event.session_id, "already-running"),
                            AgentActivityContent::Response {
                                body: "Added to the task already running for this issue."
                                    .to_owned(),
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
                return Ok(());
            }
            let body = self.boundary_reply(&report.tracker_user_id).await;
            self.config
                .automation
                .create_activity(
                    &event.session_id,
                    &stable_id(&event.session_id, "redirect"),
                    AgentActivityContent::Response { body },
                )
                .await
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        self.delegate(issue_id, &event.session_id, author, label, host)
            .await
    }

    async fn delegate(
        &self,
        issue_id: &str,
        session_id: &str,
        author: &str,
        label: &str,
        host: &dyn Host,
    ) -> Result<(), String> {
        if self.is_terminated(session_id) {
            return Ok(());
        }
        // The same session delivered again is the work already running here; it
        // must not be stopped and restarted.
        if host
            .work_for_issue(issue_id.to_owned(), vec![session_id.to_owned()])
            .await?
            .is_some()
        {
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
                let body = "A newer delegation replaced this one, so Altitude stopped this \
                            task.";
                if let Err(error) = self
                    .config
                    .automation
                    .create_activity(
                        &report.session,
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
            self.post_sync_guidance(session_id, Unprepared::UnknownOperator)
                .await?;
            return Ok(());
        };
        let peer: EndpointId = owner
            .parse()
            .map_err(|_| "the remote's key could not be read".to_owned())?;

        // Say the work is starting before the issue is fetched and a capsule is
        // provisioned. The id is stable, so a redelivery is one thought.
        self.config
            .automation
            .create_activity(
                session_id,
                &stable_id(session_id, "starting"),
                AgentActivityContent::Thought {
                    body: format!("Altitude is setting up this task for {label}."),
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
            reported_events: 0,
            finished: false,
            last_status: None,
            failed_at: None,
            failed_attempts: 0,
            unhooked_at: None,
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
            self.config
                .automation
                .create_activity(
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

    async fn answer_prompt(&self, event: &SessionEvent, host: &dyn Host) -> Result<(), String> {
        let Some(answer) = event.prompt_body.as_deref() else {
            return Ok(());
        };
        let Some(report) = self.report_for_session(&event.session_id) else {
            // No report for this session: it is either an operator's own prompt on
            // an issue still delegated to this app — which is how a session opened
            // from the issue starts work, and the retry path after a failed start —
            // or a session this machine is done with. Only a delegation the tracker
            // is actually asking for becomes work: a prompt on anything else must
            // not found a task nobody asked for.
            let Some(issue_id) = event.issue_id.as_deref() else {
                return Ok(());
            };
            let delegation = self
                .config
                .automation
                .issue_delegation(issue_id)
                .await
                .map_err(|error| error.to_string())?;
            // The tracker has to still be asking for this issue's work. Otherwise
            // the prompt is a memory of a session this machine is done with, and
            // founding a task from it is work nobody asked for.
            if delegation.delegate_id.as_deref() != Some(self.config.automation.app_user_id()) {
                return Ok(());
            }
            if let Some(author) = event.prompt_author_id.as_deref()
                && event.creator_id.as_deref() == Some(author)
                && self.owner_for_user(author).is_some()
            {
                let label = event.creator_name.as_deref().unwrap_or(author);
                return self
                    .delegate(issue_id, &event.session_id, author, label, host)
                    .await;
            }
            return Ok(());
        };
        // Only the current delegation operator's reply counts; a teammate's
        // prompt is not a decision. Say so on the session and name the operator
        // rather than silently ignoring a human who spoke to the app.
        if !event.is_from(&report.tracker_user_id) {
            let body = self.boundary_reply(&report.tracker_user_id).await;
            let discriminator = event
                .activity_id
                .as_deref()
                .unwrap_or(event.session_id.as_str());
            self.config
                .automation
                .create_activity(
                    &event.session_id,
                    &stable_id(
                        &event.session_id,
                        &format!("teammate-redirect:{discriminator}"),
                    ),
                    AgentActivityContent::Response { body },
                )
                .await
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        let signals = host.signal_threads(report.task_id.clone()).await?;
        // A prompt answers only what was already waiting when it was written. A
        // prompt that predates a signal is stale, and letting it resolve the
        // newer question would answer something the operator never saw.
        let waiting: Vec<&TaskOperatorSignalThread> = signals
            .iter()
            .filter(|signal| signal.is_unresolved())
            .filter(|signal| signal.last_raised_at_ms <= event.occurred_at_ms)
            .collect();
        // A decision is not blocking, but a reply from the issue still belongs
        // in its thread; an update is a log and is not answered here.
        let question = waiting.iter().copied().find(|signal| {
            matches!(
                signal.kind,
                TaskOperatorSignalKind::Blocked | TaskOperatorSignalKind::Decision
            )
        });
        let review = waiting
            .iter()
            .copied()
            .find(|signal| signal.kind == TaskOperatorSignalKind::InReview);
        let Some(signal) = question.or(review) else {
            return host
                .instruct(report.task_id.clone(), answer.to_owned())
                .await;
        };
        host.reply(report.task_id.clone(), signal.id.clone(), answer.to_owned())
            .await
    }

    async fn follow_issue_state(
        &self,
        issue_id: &str,
        state_id: &str,
        actor_id: &str,
        occurred_at_ms: i64,
        host: &dyn Host,
    ) -> Result<(), String> {
        let Some(report) = host
            .work_for_issue(issue_id.to_owned(), Vec::new())
            .await?
            .and_then(|task| self.report_for_task(&task.task_id))
        else {
            return Ok(());
        };
        if report.tracker_user_id.as_str() != actor_id {
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

    async fn waiting_review(
        &self,
        report: &Cursor,
        occurred_at_ms: i64,
        host: &dyn Host,
    ) -> Result<Option<TaskOperatorSignalThread>, String> {
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
            .filter(TaskOperatorSignalThread::is_unresolved)
            .find(|signal| {
                signal.kind == TaskOperatorSignalKind::InReview
                    && signal.last_raised_at_ms <= occurred_at_ms
            }))
    }

    async fn stop_for_ending(
        &self,
        host: &dyn Host,
        report: &Cursor,
        is_done: bool,
    ) -> Result<(), String> {
        let notice = if is_done {
            "You moved this issue to Done, so Altitude stopped the task: no more work is asked \
             for. Raise a new task when you want more from it."
        } else {
            "You canceled this issue, so Altitude stopped the task."
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
            .config
            .automation
            .create_activity(
                &report.session,
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

    async fn answer_comment(&self, event: &CommentInput, host: &dyn Host) -> Result<(), String> {
        let site = self
            .config
            .automation
            .comment_site(&event.comment_id)
            .await
            .map_err(|error| error.to_string())?;
        // The generated session root is Linear's own transcript surface, not a
        // signal comment. Its replies arrive as prompts on the session.
        if site.is_artificial_session_root {
            return Ok(());
        }
        let issue_id = site.issue_id.clone().or_else(|| event.issue_id.clone());
        let Some(issue_id) = issue_id else {
            return Ok(());
        };
        let Some(report) = host
            .work_for_issue(issue_id.clone(), Vec::new())
            .await?
            .and_then(|task| self.report_for_task(&task.task_id))
        else {
            return Ok(());
        };
        if !host.is_kept(report.task_id.clone()).await? {
            return Ok(());
        }
        let app_user_id = self.config.automation.app_user_id();
        let actor = if event.actor_id == report.tracker_user_id {
            Actor::Operator
        } else if event.actor_id == app_user_id {
            Actor::Agent
        } else {
            Actor::OtherHuman
        };
        // A non-operator speaking is enough to make the thread shared, even if
        // Linear's author list is incomplete.
        let has_other_humans = actor == Actor::OtherHuman
            || site.authors_truncated
            || site
                .human_authors
                .iter()
                .any(|id| id.as_str() != report.tracker_user_id && id.as_str() != app_user_id);
        // A comment on a thread this machine closed is the thread being reopened.
        // The tracker does not tell a separate story about an un-resolve, but a
        // reply in a closed thread is exactly that: nobody writes into an answered
        // question unless it is being asked again.
        self.reopen_closed_thread(&report, &site, host).await;
        // A reply inside a thread this machine published is an answer to the
        // question that thread asked, and the thread is which question it is. The
        // ids are derived from the task and the signal, so either side can work
        // them out and no table of published comments has to be kept.
        let root = site.root_comment_id.clone();
        let answered = host
            .signal_threads(report.task_id.clone())
            .await?
            .into_iter()
            .find(|thread| {
                thread.is_unresolved() && published_comment_id(&report.task_id, &thread.id) == root
            });
        let facts = InputFacts {
            actor,
            is_mention: event.is_mention,
            is_agent_thread: site.root_author_id.as_deref() == Some(app_user_id),
            is_agent_document_comment: site.document_content_id.is_some()
                && site.parent_id.is_none()
                && site.document_creator_id.as_deref() == Some(app_user_id),
            is_tracked_signal: answered.is_some(),
            is_task_issue: site.document_content_id.is_none()
                && site.issue_id.as_deref() == Some(issue_id.as_str())
                && site.parent_id.is_none(),
            has_other_humans,
        };
        let decision = admission::decide(&facts);
        tracing::info!(
            comment = %event.comment_id,
            root = %site.root_comment_id,
            site_comment = %site.comment_id,
            task = %report.task_id,
            ?decision,
            reason = decision.reason(),
            mention = event.is_mention,
            "Linear comment admitted"
        );
        match decision {
            Admission::Ignore | Admission::Context => Ok(()),
            Admission::Wake => {
                self.react_eyes(&event.comment_id).await;
                let body = if event.body.trim().is_empty() {
                    site.body.clone()
                } else {
                    event.body.clone()
                };
                if let Some(thread) = &answered {
                    // The reply answers the thread it is in. Whether an answer
                    // settles a question is the agent's to judge and it says so with
                    // its own resolve, and whether the *work* is accepted is not
                    // something words decide at all: the operator says that by
                    // moving the issue to Done, which is theirs to do and cannot be
                    // misread as a comment.
                    return host
                        .reply(report.task_id.clone(), thread.id.clone(), body)
                        .await;
                }
                let source = if site.document_id.is_some() {
                    "document"
                } else {
                    "issue"
                };
                let author = site.author_name.as_deref().unwrap_or("The operator");
                let instruction = format!(
                    "A comment from {author} on the task's {source}:\n\n{body}\n\n\
                     This comment was addressed to you. Continue the task with it."
                );
                host.instruct(report.task_id, instruction).await
            }
            Admission::Redirect => {
                self.react_eyes(&event.comment_id).await;
                let body = self.boundary_reply(&report.tracker_user_id).await;
                self.config
                    .automation
                    .comment_reply(
                        site.issue_id.as_deref(),
                        site.document_content_id.as_deref(),
                        &event.comment_id,
                        &body,
                        &stable_id("comment", &format!("{}:redirect", event.comment_id)),
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
        }
    }

    async fn react_eyes(&self, comment_id: &str) {
        let id = stable_id("reaction", &format!("{comment_id}:eyes"));
        if let Err(error) = self
            .config
            .automation
            .react_to_comment(comment_id, "eyes", Some(&id))
            .await
        {
            tracing::debug!(comment = comment_id, %error, "could not add the acknowledgement reaction");
        }
    }

    async fn boundary_reply(&self, operator_id: &str) -> String {
        let operator = self.operator_mention(operator_id).await;
        format!(
            "This task belongs to {operator}, so Altitude only takes direction from them here. \
             Ask them to reply if you want it changed."
        )
    }

    async fn summarise_session(
        &self,
        session_id: &str,
        operator_id: &str,
        agent: Option<&str>,
    ) -> Result<(), String> {
        let operator = self.operator_handle(operator_id).await;
        let agent = agent.unwrap_or("agent");
        let summary = format!("altitude / {operator}@{} / {agent}", self.config.host_label);
        self.config
            .automation
            .update_session_summary(session_id, &summary)
            .await
            .map_err(|error| error.to_string())
    }

    async fn operator_handle(&self, user_id: &str) -> String {
        match self.config.automation.user_url(user_id).await {
            Ok(url) => url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|handle| !handle.is_empty())
                .unwrap_or("operator")
                .to_owned(),
            Err(_) => "operator".to_owned(),
        }
    }

    async fn operator_mention(&self, user_id: &str) -> String {
        match self.config.automation.user_mention(user_id).await {
            Ok(mention) if !mention.is_empty() => mention,
            Ok(_) | Err(_) => {
                tracing::warn!(user = user_id, "could not read the operator's profile URL");
                "the person who delegated it".to_owned()
            }
        }
    }

    async fn poll_reports(&self, host: &dyn Host) {
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

    fn unhook_held(&self, report: &Cursor) -> Result<bool, String> {
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

    fn clear_unhook(&self, report: &Cursor) -> Result<(), String> {
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

    async fn unhook(&self, host: &dyn Host, report: &Cursor) {
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
            .config
            .automation
            .create_activity(
                &report.session,
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

    async fn sync_report(&self, host: &dyn Host, report: &Cursor) -> Result<(), String> {
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
            .filter(TaskOperatorSignalThread::is_unresolved)
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

    fn refused_event(&self, report: &Cursor, index: usize, error: String) -> Result<(), String> {
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

    async fn report_event(
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
                self.config
                    .automation
                    .create_activity(
                        &report.session,
                        // Per pause, not per task: a thread that says nothing the
                        // second time it waits is a thread the operator thinks is
                        // still thinking.
                        &stable_id(&format!("{}-{index}", report.task_id), "paused"),
                        AgentActivityContent::Response {
                            body: pause_notice(execution_pause_reason).to_owned(),
                        },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            TaskHistoryEvent::PlanUpdated { steps } => {
                self.config
                    .automation
                    .update_session_plan(&report.session, steps)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            TaskHistoryEvent::AgentRunStarted { agent_run_id, .. } => {
                if events[..index].iter().any(|(_, event)| {
                    matches!(
                        event,
                        TaskHistoryEvent::OperatorSignalMessage {
                            author: TaskOperatorSignalAuthor::Operator,
                            ..
                        }
                    )
                }) {
                    self.config
                        .automation
                        .create_activity(
                            &report.session,
                            &stable_id(&report.task_id, agent_run_id),
                            AgentActivityContent::Thought {
                                body: "Got your reply. Altitude is continuing the task.".into(),
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
                self.write_status(host, report, TrackerStatus::InProgress)
                    .await?;
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
                self.follow_signal_thread(report, signal_id, &events[..=index], true)
                    .await;
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
                self.follow_signal_thread(report, signal_id, &events[..=index], false)
                    .await;
                if !self.is_settled(&report.issue_id)
                    && let Err(error) = self
                        .write_status(host, report, TrackerStatus::InReview)
                        .await
                {
                    tracing::warn!(issue = %report.issue_id, %error, "could not say the task is waiting again");
                }
            }
            TaskHistoryEvent::OperatorSignalConfirmed { signal_id } => {
                self.follow_signal_thread(report, signal_id, &events[..=index], true)
                    .await;
                self.finish_accepted(host, report, &events[..=index])
                    .await?;
                return Ok(Some(()));
            }
            TaskHistoryEvent::TaskCompleted { .. } => {
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

    async fn replay_signals(
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

    async fn finish_transferred(&self, report: &Cursor) -> Result<(), String> {
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

    async fn publish_signal(
        &self,
        report: &Cursor,
        signal_id: &str,
        kind: TaskOperatorSignalKind,
        title: &str,
        body: &str,
        contributions: &[String],
    ) -> Result<Option<TrackerStatus>, String> {
        let comment_id = published_comment_id(&report.task_id, signal_id);
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

    async fn follow_signal_thread(
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

    async fn resolve_signal_comments(&self, report: &Cursor, events: &[(i64, TaskHistoryEvent)]) {
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

    async fn write_status(
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

    fn forget_status(&self, issue_id: &str) -> Result<(), String> {
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

    async fn say_task_is_running(&self, host: &dyn Host, report: &Cursor) {
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

    async fn finish_accepted(
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

    async fn post_sync_guidance(&self, session_id: &str, cause: Unprepared) -> Result<(), String> {
        let tracker = TrackerKind::Linear.label();
        let host = whoami::hostname().unwrap_or_else(|_| "this remote".to_owned());
        // Two different things stop a delegation here, and naming the wrong one sends
        // the operator to a setting that is already right.
        let body = match cause {
            Unprepared::NoProjects => format!(
                "**This remote has no projects yet.**\n\n\
                 No one has added this remote to a project in Altitude, so it has no code, \
                 no agent, and no settings to run this issue with.\n\n\
                 On your computer:\n\n\
                 1. Open Altitude, or install it if you don't have it.\n\
                 2. In Settings → Remotes, add this remote: {host}\n\
                 3. In Settings → Task tracker, sign in to {tracker}, so Altitude knows \
                 whose work this is.\n\n\
                 Altitude passes your setup to this remote in the background. Reply here \
                 once that's done, and this issue gets picked up."
            ),
            Unprepared::UnknownOperator => format!(
                "**Altitude does not know whose work this is.**\n\n\
                 This remote holds no {tracker} login from a person it could report to, so \
                 it cannot start a task for you.\n\n\
                 On your computer, in Settings → Task tracker, sign in to {tracker} as \
                 yourself. The login is passed to this remote in the background; reply \
                 here once that's done."
            ),
        };
        self.config
            .automation
            .create_activity(
                session_id,
                &stable_id(session_id, "sync-error"),
                AgentActivityContent::Error { body },
            )
            .await
            .map_err(|error| error.to_string())
    }

    async fn post_start_guidance(
        &self,
        session_id: &str,
        refusal: &StartRefusal,
    ) -> Result<(), String> {
        let body = format!(
            "**Nothing was started.**\n\n{refusal}\n\n\
             Reply here once that's sorted, and Altitude will try again."
        );
        self.config
            .automation
            .create_activity(
                session_id,
                &stable_id(session_id, "start-error"),
                AgentActivityContent::Error { body },
            )
            .await
            .map_err(|error| error.to_string())
    }

    fn owner_for_user(&self, tracker_user_id: &str) -> Option<String> {
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

    fn user_for_owner(&self, owner: &str) -> Option<String> {
        logins(&self.config.operators_dir)
            .into_iter()
            .find(|login| login.owner == owner)
            .map(|login| login.account.user_id)
    }

    fn settle(&self, issue_id: &str) -> Result<(), String> {
        self.mutate(|ledger| {
            if !ledger.settled_issues.iter().any(|id| id == issue_id) {
                ledger.settled_issues.push(issue_id.to_owned());
            }
        })
    }

    fn is_settled(&self, issue_id: &str) -> bool {
        self.lock()
            .map(|ledger| ledger.settled_issues.iter().any(|id| id == issue_id))
            .unwrap_or(false)
    }

    fn report_for_task(&self, task_id: &str) -> Option<Cursor> {
        let ledger = self.lock().ok()?;
        ledger
            .cursors
            .iter()
            .find(|report| report.task_id == task_id)
            .cloned()
    }

    fn report_for_session(&self, session_id: &str) -> Option<Cursor> {
        let ledger = self.lock().ok()?;
        ledger
            .cursors
            .iter()
            .find(|report| report.session == session_id)
            .cloned()
    }

    fn cursors(&self) -> Vec<Cursor> {
        self.lock()
            .map(|ledger| ledger.cursors.clone())
            .unwrap_or_default()
    }

    fn upsert_report(&self, report: Cursor) -> Result<(), String> {
        self.mutate(|ledger| {
            ledger
                .cursors
                .retain(|existing| existing.task_id != report.task_id);
            ledger.cursors.push(report);
        })
    }

    fn finish_report(&self, report: &Cursor) -> Result<(), String> {
        self.mutate(|ledger| {
            for existing in &mut ledger.cursors {
                if existing.task_id == report.task_id {
                    existing.finished = true;
                }
            }
        })
    }

    fn retire_report(&self, report: &Cursor) -> Result<(), String> {
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

    fn is_terminated(&self, session_id: &str) -> bool {
        self.lock()
            .map(|ledger| {
                ledger
                    .terminated_sessions
                    .iter()
                    .any(|session| session.session_id == session_id)
            })
            .unwrap_or(false)
    }

    fn advance_cursor(&self, task_id: &str, cursor: usize) -> Result<(), String> {
        self.mutate(|ledger| {
            if let Some(report) = ledger.cursors.iter_mut().find(|r| r.task_id == task_id) {
                report.reported_events = cursor;
            }
        })
    }

    fn issue_reference(&self, issue_id: &str) -> IssueReference {
        IssueReference {
            provider: TrackerKind::Linear,
            workspace_id: self.config.automation.workspace_id().to_owned(),
            id: issue_id.to_owned(),
            identifier: String::new(),
            url: String::new(),
        }
    }
}

// ------------------------------------------------------------------ the wire

async fn webhook(
    State(tracker): State<Arc<RemoteTracker>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok());
    if !verify_signature(&tracker.config.webhook_secret, signature, &body) {
        // Loud, and without the value: a secret that does not match the app's is
        // the one misconfiguration that otherwise looks like Linear sending
        // nothing at all.
        tracing::warn!(
            header = signature.is_some(),
            "Linear delivery rejected · its signature does not match this machine's webhook secret"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let event = match classify_webhook(&tracker, &body) {
        WebhookOutcome::Store(event) => event,
        // A well-formed webhook for another resource is not this coordinator's
        // work. Acknowledging it stops Linear retrying it forever.
        WebhookOutcome::Ignore(reason) => {
            tracing::debug!(reason, "Linear delivery ignored");
            return StatusCode::OK.into_response();
        }
        WebhookOutcome::Reject(reason) => {
            tracing::warn!(%reason, "Linear delivery rejected");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // The line says what arrived, never what it said: a delivery carries the
    // operator's own words, and a log is not the place to keep them.
    let delivery = event.describe();
    // Identity comes from the signed body, not the unsigned delivery header:
    // a header a sender can choose is transport, not what the event is.
    let key = event.dedupe_key();
    match tracker.enqueue(key, event) {
        Ok(true) if !tracker.delivered.swap(true, Ordering::Relaxed) => tracing::info!(
            %delivery,
            "Linear delivery received · the first one since this machine started"
        ),
        Ok(true) => tracing::debug!(%delivery, "Linear delivery recorded"),
        Ok(false) => tracing::debug!(%delivery, "Linear delivery was already recorded"),
        Err(error) => {
            tracing::error!(%error, "could not durably record a Linear delivery");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    StatusCode::OK.into_response()
}

// Why a delegation here cannot be started. Two answers, because they point at two
// different settings and only one of them is the operator's to fix.
#[derive(Clone, Copy)]
enum Unprepared {
    NoProjects,
    UnknownOperator,
}

enum WebhookOutcome {
    Store(StoredEvent),
    Ignore(&'static str),
    Reject(String),
}

fn classify_webhook(tracker: &RemoteTracker, body: &[u8]) -> WebhookOutcome {
    let Some(event_type) = webhook_event_type(body) else {
        return WebhookOutcome::Reject("the body has no event type".to_owned());
    };
    match event_type.as_str() {
        "AgentSessionEvent" => match parse_agent_session_event(body) {
            Err(WebhookError::UnsupportedEvent(_)) => {
                WebhookOutcome::Ignore("an agent session action this coordinator does not run")
            }
            Err(error) => WebhookOutcome::Reject(format!(
                "the agent session event could not be read: {error}"
            )),
            Ok(event) => {
                if event
                    .validate(
                        tracker.config.automation.workspace_id(),
                        tracker.config.automation.app_user_id(),
                    )
                    .is_err()
                {
                    return WebhookOutcome::Reject("another workspace or app".to_owned());
                }
                if event.oauth_client_id != tracker.config.oauth_client_id {
                    return WebhookOutcome::Reject("another OAuth application".to_owned());
                }
                WebhookOutcome::Store(StoredEvent::Session(Box::new(SessionEvent::from(&event))))
            }
        },
        "Issue" => match parse_issue_event(body) {
            Err(error) => {
                WebhookOutcome::Reject(format!("the issue event could not be read: {error}"))
            }
            Ok(event) => {
                if event.organization_id != tracker.config.automation.workspace_id() {
                    return WebhookOutcome::Reject("another workspace".to_owned());
                }
                if !event.is_fresh() {
                    return WebhookOutcome::Reject(
                        "older than the freshness window · check this machine's clock".to_owned(),
                    );
                }
                match event.into_stored() {
                    Some(stored) => WebhookOutcome::Store(stored),
                    None => WebhookOutcome::Ignore("not a state change a person made"),
                }
            }
        },
        "AppUserNotification" => match parse_app_notification(body) {
            Err(error) => {
                WebhookOutcome::Reject(format!("the notification event could not be read: {error}"))
            }
            Ok(event) => {
                if event.organization_id != tracker.config.automation.workspace_id() {
                    return WebhookOutcome::Reject("another workspace".to_owned());
                }
                if event.app_user_id != tracker.config.automation.app_user_id() {
                    return WebhookOutcome::Reject("another app user".to_owned());
                }
                if event
                    .oauth_client_id
                    .as_deref()
                    .is_some_and(|id| id != tracker.config.oauth_client_id)
                {
                    return WebhookOutcome::Reject("another OAuth application".to_owned());
                }
                if !event.is_fresh() {
                    return WebhookOutcome::Reject(
                        "older than the freshness window · check this machine's clock".to_owned(),
                    );
                }
                match event.into_stored() {
                    Some(stored) => WebhookOutcome::Store(stored),
                    None => WebhookOutcome::Ignore("a notification that carries no comment"),
                }
            }
        },
        _ => WebhookOutcome::Ignore("not a resource this coordinator handles"),
    }
}

fn webhook_event_type(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type")]
        event_type: String,
    }
    serde_json::from_slice::<Envelope>(body)
        .ok()
        .map(|envelope| envelope.event_type)
}

impl SessionEvent {
    fn is_stop(&self) -> bool {
        if self.signal.as_deref() == Some("stop") {
            return true;
        }
        let action = self.action.to_ascii_lowercase();
        if [
            "cancel",
            "canceled",
            "cancelled",
            "stop",
            "stopped",
            "abort",
            "aborted",
        ]
        .contains(&action.as_str())
        {
            return true;
        }
        if let Some(body) = self.prompt_body.as_deref() {
            let body = body.trim().to_ascii_lowercase();
            if ["stop", "cancel", "cancelled", "canceled"].contains(&body.as_str()) {
                return true;
            }
        }
        false
    }

    fn is_from(&self, tracker_user_id: &str) -> bool {
        self.prompt_author_id.as_deref() == Some(tracker_user_id)
    }

    fn dedupe_key(&self) -> String {
        if self.is_stop() {
            return format!(
                "session:{}:stop:{}",
                self.session_id,
                self.activity_id.as_deref().unwrap_or_default()
            );
        }
        match self.action.as_str() {
            "created" => format!("session:{}:created", self.session_id),
            "prompted" => match &self.activity_id {
                Some(activity_id) => format!("activity:{activity_id}"),
                None => format!(
                    "prompt:{}:{}",
                    self.session_id,
                    self.prompt_body.as_deref().unwrap_or_default()
                ),
            },
            other => format!("session:{}:{other}", self.session_id),
        }
    }
}

impl From<&AgentSessionEvent> for SessionEvent {
    fn from(event: &AgentSessionEvent) -> Self {
        let activity = event.agent_activity.as_ref();
        Self {
            action: event.action.clone(),
            session_id: event.agent_session.id.clone(),
            issue_id: event.agent_session.issue_id.clone(),
            comment_id: event.agent_session.comment_id.clone(),
            creator_id: event.delegation_author_id().map(str::to_owned),
            creator_name: event
                .agent_session
                .creator
                .as_ref()
                .map(|creator| creator.name.clone()),
            prompt_author_id: event.prompt_author_id().map(str::to_owned),
            prompt_body: activity.and_then(|activity| activity.content.body.clone()),
            signal: activity.and_then(|activity| activity.signal.clone()),
            source_comment_id: event.agent_session.source_comment_id.clone(),
            activity_id: activity.map(|activity| activity.id.clone()),
            occurred_at_ms: event.webhook_timestamp as i64,
        }
    }
}

// --------------------------------------------------------------------- utility

fn persist(path: &Path, ledger: &Ledger) -> Result<(), String> {
    let text = serde_json::to_string_pretty(ledger)
        .map_err(|error| format!("could not encode the tracker ledger: {error}"))?;
    write_private(path, &text)
        .map_err(|error| format!("could not write the tracker ledger: {error}"))
}

fn signal_kind(
    events: &[(i64, TaskHistoryEvent)],
    signal_id: &str,
) -> Option<TaskOperatorSignalKind> {
    events.iter().rev().find_map(|(_, event)| match event {
        TaskHistoryEvent::OperatorSignalRaised { signal } if signal.id == signal_id => {
            Some(signal.kind)
        }
        _ => None,
    })
}

fn stable_id(namespace: &str, discriminator: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(namespace.as_bytes());
    hasher.update(b"\0");
    hasher.update(discriminator.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    // Linear requires a v4 UUID; set the version and variant bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn was_transferred(events: &[(i64, TaskHistoryEvent)]) -> bool {
    events.iter().any(|(_, event)| {
        matches!(
            event,
            TaskHistoryEvent::TransferStaged { .. }
                | TaskHistoryEvent::TransferLanded { .. }
                | TaskHistoryEvent::TransferCommitted { .. }
        )
    })
}

fn contribution_urls(events: &[(i64, TaskHistoryEvent)]) -> Vec<String> {
    events
        .iter()
        .filter_map(|(_, event)| match event {
            TaskHistoryEvent::CodeContributionLinked { contribution } => {
                Some(contribution.url.clone())
            }
            _ => None,
        })
        .collect()
}

fn contribution_details(events: &[(i64, TaskHistoryEvent)]) -> Vec<String> {
    events
        .iter()
        .filter_map(|(_, event)| match event {
            TaskHistoryEvent::CodeContributionLinked { contribution } => {
                let latest = events.iter().rev().find_map(|(_, event)| match event {
                    TaskHistoryEvent::CodeContributionObserved { observation }
                        if observation.contribution_id == contribution.id =>
                    {
                        Some(observation.summary.as_str())
                    }
                    _ => None,
                });
                Some(match latest {
                    Some(summary) => format!("{} — {summary}", contribution.url),
                    None => contribution.url.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

fn append_contributions(body: &mut String, contributions: &[String]) {
    if contributions.is_empty() {
        return;
    }
    body.push_str("\n\n");
    let list = contributions
        .iter()
        .map(|url| format!("- {url}"))
        .collect::<Vec<_>>()
        .join("\n");
    body.push_str(&list);
}

fn accepted_body(contributions: &[String]) -> String {
    let mut body = "Accepted. The work is complete.".to_owned();
    append_contributions(&mut body, contributions);
    body
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}
