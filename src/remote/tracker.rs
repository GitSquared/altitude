//! The remote's Linear coordinator: signed webhooks in, task host out.
//!
//! A serve role has no person at the keyboard. Work arrives as a Linear
//! AgentSessionEvent, an Issue status change, or a comment notification, is
//! durably recorded before the webhook is acknowledged, and is then handed to the
//! task host that owns the capsule. Comments pass through
//! [`super::admission`] before anything reacts or wakes. This module owns only the
//! mapping between an external issue/session and a local task, a client, and a
//! tracker user — one [`IssueReport`] per task — and the task's own state is
//! always read back from the host, never cached here.
//!
//! The coordinator calls the host through [`Host`], a small callback surface the
//! serve role implements over its task event loop. Keeping the two apart is what
//! lets the execution core stay the source of operational truth.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fs,
    future::Future,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
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
        ACCOUNT_FILE, IssueReference, IssueSnapshot, TrackerAccount, TrackerKind, TrackerStatus,
        automation::{
            AgentActivityContent, AgentSessionEvent, CommentSite, LinearAutomation, WebhookError,
            parse_agent_session_event, verify_signature,
        },
    },
};

use super::{
    admission::{self, Actor, Admission, InputFacts},
    identity::write_private,
};

const WEBHOOK_ROUTE: &str = "/altitude/linear/webhook";
const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;
const SIGNATURE_HEADER: &str = "linear-signature";

const TICK: Duration = Duration::from_secs(10);
/// How many passes may fail on one event before it is skipped. A tracker that will
/// not take an event is news; a machine asking again forever is not.
const EVENT_ATTEMPTS: usize = 3;
/// How long a lapsed delegation is waited on before it ends the task.
const UNHOOK_GRACE_MS: i64 = 60_000;
/// A failed delivery is retried at a fixed pace: the work is already durable on
/// disk, so a tracker outage should not produce a request storm.
const RETRY: Duration = Duration::from_secs(10);
/// Completed deliveries and terminated sessions are kept long enough that a
/// redelivery or a replay is still recognised.
const DONE_RETENTION: Duration = Duration::from_secs(600);
const TERMINATED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DONE: usize = 512;
const MAX_TERMINATED: usize = 1024;
const WEBHOOK_MAX_AGE: Duration = Duration::from_secs(300);

/// Everything the coordinator needs that the serve role already owns.
///
/// The token, workspace, and app user live inside [`LinearAutomation`], which
/// is why they are not repeated here: a webhook is validated against the same
/// handle the API calls go through, so the two cannot disagree.
pub struct RemoteTrackerConfig {
    pub automation: Arc<LinearAutomation>,
    pub webhook_secret: Vec<u8>,
    pub oauth_client_id: String,
    pub storage_path: PathBuf,
    /// Where each peer's pushed configuration lives, because that is where the
    /// account a peer is signed in as is written down.
    pub operators_dir: PathBuf,
    pub host_label: String,
}

/// The task operations the coordinator asks of the machine that owns the capsule.
///
/// Every method returns a boxed future so the trait stays object-safe without
/// `async fn`, matching the tracker provider's own shape. The host derives each
/// answer from the task's own history; nothing here is a cache.
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub trait Host: Send + Sync + 'static {
    /// Found a task for `issue` on the machine reached through `peer`.
    ///
    /// The host chooses the environment and agent: a remote runs what it was
    /// pushed, and the client does not get to name it. The label is the signed
    /// human name the work belongs to, so a task card reads a person rather than
    /// the peer's key.
    fn start_issue(
        &self,
        peer: EndpointId,
        issue: IssueSnapshot,
        label: String,
    ) -> super::StartFuture<'_, String>;
    /// Stop a task this machine no longer runs. The reason is kept in the task's
    /// own record, so an ending that happened here can be read here.
    fn stop(&self, task_id: String, reason: TaskExecutionPauseReason) -> HostFuture<'_, ()>;
    /// Record in the task's own history that the issue now says `status`, so a
    /// card can show what the tracker says instead of asking it again.
    fn record_status(&self, task_id: String, status: TrackerStatus) -> HostFuture<'_, ()>;
    /// Every thread this task raised, open and closed. The caller filters: which
    /// ones are waiting is a question about the answer, not about the task.
    fn signal_threads(&self, task_id: String) -> HostFuture<'_, Vec<TaskOperatorSignalThread>>;
    /// Reopen a thread the operator has come back to, so this machine's state
    /// follows the conversation rather than remembering an answer it no longer has.
    fn reopen_signal(&self, task_id: String, signal_id: String) -> HostFuture<'_, ()>;
    fn events(&self, task_id: String) -> HostFuture<'_, Vec<(i64, TaskHistoryEvent)>>;
    fn instruct(&self, task_id: String, instruction: String) -> HostFuture<'_, ()>;
    fn reply(&self, task_id: String, signal_id: String, answer: String) -> HostFuture<'_, ()>;
    fn accept(&self, task_id: String, signal_id: String) -> HostFuture<'_, ()>;
    /// Whether the task's record is still kept on this machine. A completed task
    /// is kept, which is what lets its ending be reported before it is dropped.
    fn is_kept(&self, task_id: String) -> HostFuture<'_, bool>;

    /// The tasks here that are bound to a tracker issue, with the client whose
    /// work each one is. Reporting is adopted from this list.
    fn bound_tasks(&self) -> HostFuture<'_, Vec<BoundTask>>;
    fn session_agent(&self, task_id: String) -> HostFuture<'_, Option<String>>;
}

/// The tracker comment this machine publishes a signal thread under.
///
/// Derived rather than recorded: both sides of the conversation can work it out
/// from the task and the signal, so there is no table to keep in step.
fn published_comment_id(task_id: &str, signal_id: &str) -> String {
    stable_id("comment", &format!("signal:{task_id}:{signal_id}"))
}

/// What the session says when a task stops working, by why it stopped.
fn pause_notice(reason: &TaskExecutionPauseReason) -> &'static str {
    match reason {
        TaskExecutionPauseReason::AwaitingOperator => "Waiting for your reply on this task.",
        TaskExecutionPauseReason::AwaitingReviewChecks => {
            "Waiting for the pull request checks to settle."
        }
        TaskExecutionPauseReason::ApplicationShutdown => {
            "The machine running this task restarted. The task resumes when it is back."
        }
        TaskExecutionPauseReason::Transfer => {
            "This task is moving to another machine and keeps running there."
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

/// Whether asking a tracker again could work.
///
/// A revoked token, a rate limit, and a tracker having a bad day are all worth
/// another pass. An issue the tracker no longer has is not: the task will not
/// become reportable by waiting.
fn worth_retrying(refusal: &str) -> bool {
    !refusal.contains("not found") && !refusal.contains("Entity not found")
}

/// A task this machine holds that is bound to a tracker issue.
pub struct BoundTask {
    pub task_id: String,
    /// The client whose login reports it, as the peer id it arrived as.
    pub owner: String,
    pub issue: IssueReference,
    pub session_agent: Option<String>,
}

pub struct RemoteTracker {
    config: RemoteTrackerConfig,
    ledger: Mutex<Ledger>,
    notify: Notify,
    /// Why a task could not be adopted, so a tracker that keeps refusing one task
    /// says so once instead of every tick. A task is still retried: the reason may
    /// go away, and silence about a failure is worse than a second line about it.
    refusals: Mutex<HashMap<String, String>>,
    /// Whether anything has arrived since this process started. The first
    /// delivery is said at info because it is the only proof a freshly installed
    /// app can reach this machine, and a silent first install is indistinguishable
    /// from a broken proxy.
    delivered: AtomicBool,
}

impl RemoteTracker {
    /// Open the coordinator over an already-verified API handle.
    ///
    /// A missing ledger starts empty; one that cannot be read is an error, so a
    /// corrupt file is never silently replaced by an empty one.
    pub fn new(config: RemoteTrackerConfig) -> Result<Arc<Self>, String> {
        let ledger = Ledger::load(&config.storage_path)?;
        Ok(Arc::new(Self {
            config,
            ledger: Mutex::new(ledger),
            refusals: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            delivered: AtomicBool::new(false),
        }))
    }

    /// The HTTP surface a serve role mounts beside its other routes.
    ///
    /// The body limit is applied here rather than trusted from the caller, so a
    /// router built without care cannot hand the handler an unbounded body.
    pub fn webhook_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(WEBHOOK_ROUTE, post(webhook))
            .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BYTES))
            .with_state(Arc::clone(self))
    }

    /// Sponsor a session on an issue for a task a client started here.
    ///
    /// The peer must have synced a tracker account: a report needs a tracker user
    /// who can answer it, and a peer that has pushed none has nobody. The session
    /// is made through the public Agent API so an issue this app was never
    /// delegated can still be reported on, and the human assignee is left alone. A
    /// report that already holds the task or the issue is left as it is, so a retry
    /// cannot sponsor a second session once one has been recorded.
    pub async fn start_reporting(
        &self,
        owner: &str,
        task_id: String,
        issue: IssueReference,
        session_agent: Option<String>,
    ) -> Result<(), String> {
        let Some(tracker_user_id) = self.user_for_owner(owner) else {
            return Err("sync a Linear account to this remote before delegating issue work".into());
        };
        if self.report_for_task(&task_id).is_some()
            || self
                .report_for_issue(&issue.id)
                .is_some_and(|report| !report.finished)
        {
            return Ok(());
        }
        let session_id = self
            .config
            .automation
            .create_session_on_issue(&issue.id)
            .await
            .map_err(|error| error.to_string())?;
        self.upsert_report(IssueReport {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            tracker_session_id: session_id.clone(),
            owner: owner.to_owned(),
            tracker_user_id: tracker_user_id.clone(),
            task_id,
            reported_events: 0,
            origin: ReportOrigin::StartedByClient,
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
            .set_session_summary(&session_id, &tracker_user_id, session_agent.as_deref())
            .await
        {
            tracing::warn!(session = %session_id, %error, "could not name a piloted session");
        }
        Ok(())
    }

    /// A task was deleted here: close what the tracker knows about it.
    ///
    /// Deleting a task is the operator saying the work is gone, so the issue stops
    /// pointing at this machine and the report closes rather than waiting for a
    /// history that will never advance. Best-effort by design: the task is already
    /// deleted, and an unreachable tracker must not resurrect it.
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
                &report.tracker_session_id,
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

    /// Drain queued webhooks and report task life, forever.
    ///
    /// A webhook wakes the loop at once; a tracker or API outage is logged and
    /// retried on the next pass, and never stops the loop, because the work it is
    /// reporting on keeps running.
    pub async fn run(self: Arc<Self>, host: Arc<dyn Host>) {
        let mut tick = interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = self.notify.notified() => {}
            }
            self.process_deliveries(host.as_ref()).await;
            self.adopt_bound_tasks(host.as_ref()).await;
            self.poll_reports(host.as_ref()).await;
        }
    }

    /// Start reporting on the tasks that are bound to an issue and are not being
    /// reported on.
    ///
    /// A report is written when the task is created, which is one chance: a task
    /// created while this machine held no account for its client had none, and a
    /// ledger that was lost takes every report with it. Reading the tasks this
    /// machine holds is what gives it a second chance.
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
            if self.report_for_task(&task.task_id).is_some()
                || self
                    .report_for_issue(&task.issue.id)
                    .is_some_and(|report| !report.finished)
                || self.user_for_owner(&task.owner).is_none()
            {
                continue;
            }
            let outcome = self
                .start_reporting(
                    &task.owner,
                    task.task_id.clone(),
                    task.issue.clone(),
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

    /// Change the ledger and write it before the change is visible in memory.
    ///
    /// A failed write leaves the previous ledger in place, so a caller that gets
    /// an error knows nothing was recorded and can retry.
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

    /// Accept a signed delivery before the webhook is acknowledged.
    ///
    /// Returns whether the delivery is new; a redelivery is acknowledged without
    /// being queued twice.
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

    /// Handle every pending delivery that is due, in arrival order.
    ///
    /// Each delivery stands on its own: one that fails is deferred and the rest
    /// still run, so a single bad issue cannot block another operator's stop or
    /// cancellation.
    async fn process_deliveries(&self, host: &dyn Host) {
        for delivery in self.pending() {
            if delivery.next_attempt_at_ms > now_ms() {
                continue;
            }
            match self.handle_delivery(&delivery.event, host).await {
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

    async fn handle_delivery(&self, event: &StoredEvent, host: &dyn Host) -> Result<(), String> {
        match event {
            StoredEvent::Session(session) => {
                if session.is_stop() {
                    return self.handle_stop(session, host).await;
                }
                match session.action.as_str() {
                    "created" => self.handle_created(session, host).await,
                    "prompted" => self.handle_prompted(session, host).await,
                    _ => Ok(()),
                }
            }
            StoredEvent::IssueStateChanged(change) => {
                self.handle_issue_state(
                    &change.issue_id,
                    &change.state_id,
                    &change.actor_id,
                    change.occurred_at_ms,
                    host,
                )
                .await
            }
            StoredEvent::Comment(comment) => self.handle_comment(comment, host).await,
        }
    }

    /// Reopen the thread a comment landed on, when this machine had closed it.
    ///
    /// The comment is matched to its signal by the id this machine gave the thread
    /// it published, so no table of published comments has to be kept: the ids are
    /// derived from the task and the signal, and either side can work them out.
    async fn reopen_closed_thread(
        &self,
        report: &IssueReport,
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

    /// A stop is a cancellation, never an acceptance or a status restore.
    ///
    /// Only the delegation operator may stop the work; a teammate's stop is not the
    /// operator's judgment and is ignored. The report closes only after the stop and
    /// its notice have both succeeded, so nothing writes to it again.
    async fn handle_stop(&self, event: &SessionEvent, host: &dyn Host) -> Result<(), String> {
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
                &report.tracker_session_id,
                &stable_id(&report.task_id, "stopped"),
                AgentActivityContent::Response {
                    body: "Stopped. Altitude ended this task, deleted its sandbox, and handed \
                           the issue back."
                        .to_owned(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        self.finish_report(&report)
    }

    async fn handle_created(&self, event: &SessionEvent, host: &dyn Host) -> Result<(), String> {
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
            if self.report_for_issue(issue_id).is_none() {
                self.post_sync_guidance(&event.session_id).await?;
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
        if let Some(report) = self.report_for_issue(issue_id)
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

    /// Start (or restart) the task for an issue under `author`'s remote.
    ///
    /// Shared by a `created` delegation and by the retry an operator's prompt
    /// makes, so both paths behave identically and a failed start can be tried
    /// again.
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
        if self.report_for_session(session_id).is_some() {
            return Ok(());
        }

        // A different session on an issue that already has work is a
        // redelegation: stop the old task before the new one starts, so two
        // capsules never own the same issue.
        let redelegation = if let Some(existing) = self.report_for_issue(issue_id) {
            host.stop(
                existing.task_id.clone(),
                TaskExecutionPauseReason::StoppedInTracker,
            )
            .await?;
            self.config
                .automation
                .create_activity(
                    &existing.tracker_session_id,
                    &stable_id(&existing.task_id, "redelegated"),
                    AgentActivityContent::Response {
                        body:
                            "A newer delegation replaced this one, so Altitude stopped this task."
                                .into(),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            self.retire_report(&existing)?;
            true
        } else {
            false
        };

        let Some(owner) = self.owner_for_user(author) else {
            self.post_sync_guidance(session_id).await?;
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
        let task_id = match host.start_issue(peer, snapshot, label.to_owned()).await {
            Ok(task_id) => task_id,
            Err(refusal) => {
                // A remote that cannot start the work needs its operator to push
                // a setup. Retrying forever would wedge every later delivery.
                self.post_start_guidance(session_id, &refusal).await?;
                return Ok(());
            }
        };
        self.upsert_report(IssueReport {
            issue_id: issue_id.to_owned(),
            issue_identifier: self.issue_reference(issue_id).identifier.clone(),
            tracker_session_id: session_id.to_owned(),
            owner,
            tracker_user_id: author.to_owned(),
            task_id: task_id.clone(),
            reported_events: 0,
            origin: ReportOrigin::Delegated,
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
            .set_session_summary(session_id, author, session_agent.as_deref())
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

    async fn handle_prompted(&self, event: &SessionEvent, host: &dyn Host) -> Result<(), String> {
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
        if signal.kind == TaskOperatorSignalKind::InReview && is_acceptance(answer) {
            host.accept(report.task_id.clone(), signal.id.clone())
                .await?;
            let events = host.events(report.task_id.clone()).await?;
            return self.finish_accepted(host, &report, &events).await;
        }
        host.reply(report.task_id.clone(), signal.id.clone(), answer.to_owned())
            .await
    }

    /// The operator moved an issue's workflow state.
    ///
    /// Only the operator's own change is the operator's judgment — a teammate's
    /// status change is not a decision about their work — and only a state the
    /// tracker itself reports as an ending closes this cycle.
    ///
    /// An ending means the issue is finished or canceled, and there are two ways to
    /// honour it: a review that was waiting is accepted, because moving the issue
    /// to Done is how a person says yes; anything else stops the task, because no
    /// more work is being asked for. Both record the ending first, so nothing here
    /// writes over it afterwards.
    async fn handle_issue_state(
        &self,
        issue_id: &str,
        state_id: &str,
        actor_id: &str,
        occurred_at_ms: i64,
        host: &dyn Host,
    ) -> Result<(), String> {
        let Some(report) = self.report_for_issue(issue_id) else {
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

    /// The review signal that was already waiting when the issue was closed.
    ///
    /// A review raised after the operator moved the issue belongs to a later cycle,
    /// and a task that was not waiting on one is not accepting anything.
    async fn waiting_review(
        &self,
        report: &IssueReport,
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

    /// Stop a task because the operator ended its issue.
    ///
    /// The session says what happened, so the tracker does not look like it is
    /// still thinking while the sandbox disappears.
    async fn stop_for_ending(
        &self,
        host: &dyn Host,
        report: &IssueReport,
        is_done: bool,
    ) -> Result<(), String> {
        let notice = if is_done {
            "You moved this issue to Done, so Altitude stopped the task: no more work is asked \
             for. Raise a new task when you want more from it."
        } else {
            "You canceled this issue, so Altitude stopped the task."
        };
        host.stop(
            report.task_id.clone(),
            TaskExecutionPauseReason::TrackerEnded,
        )
        .await?;
        if let Err(error) = self
            .config
            .automation
            .create_activity(
                &report.tracker_session_id,
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

    /// A comment reached a task's issue or document. Decide whether it is for
    /// the agent before reacting or waking anyone.
    async fn handle_comment(&self, event: &CommentInput, host: &dyn Host) -> Result<(), String> {
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
        let Some(report) = self.report_for_issue(&issue_id) else {
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
        let facts = InputFacts {
            actor,
            is_mention: event.is_mention,
            is_agent_thread: site.root_author_id.as_deref() == Some(app_user_id),
            is_agent_document_comment: site.document_content_id.is_some()
                && site.parent_id.is_none()
                && site.document_creator_id.as_deref() == Some(app_user_id),
            // Signal comments do not exist yet; the binding ledger will set this
            // when they do.
            is_tracked_signal: false,
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

    async fn set_session_summary(
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
        match self.config.automation.user_url(user_id).await {
            Ok(url) if !url.is_empty() => url,
            Ok(_) | Err(_) => {
                tracing::warn!(user = user_id, "could not read the operator's profile URL");
                "the person who delegated it".to_owned()
            }
        }
    }

    /// Report task life outward, and stop work the issue no longer delegates.
    ///
    /// A completed issue is not accepted here: a poll cannot prove who changed
    /// the state. It is only ignored, so it never blocks the other tasks'
    /// reporting.
    async fn poll_reports(&self, host: &dyn Host) {
        for report in self.reports().into_iter().filter(|report| !report.finished) {
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
            // A piloted report was not delegated to the app, so it does not end
            // when the issue stops being delegated; it still accepts a native
            // stop and the operator's own control.
            if report.origin == ReportOrigin::Delegated {
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

    /// Whether a delegation that lapsed has stayed lapsed long enough to act on.
    ///
    /// The first time it is seen, the lapse is recorded rather than acted on: a
    /// toggle the operator was still making up their mind about would otherwise
    /// stop a task and start another one a moment later.
    fn unhook_held(&self, report: &IssueReport) -> Result<bool, String> {
        let now = now_ms();
        let mut held_for = None;
        self.mutate(|ledger| {
            if let Some(held) = ledger
                .reports
                .iter_mut()
                .find(|held| held.task_id == report.task_id)
            {
                let at = *held.unhooked_at.get_or_insert(now);
                held_for = Some(now - at);
            }
        })?;
        Ok(held_for.is_some_and(|held_for| held_for >= UNHOOK_GRACE_MS))
    }

    /// The delegation is back, so the lapse it lapsed from never happened.
    fn clear_unhook(&self, report: &IssueReport) -> Result<(), String> {
        if report.unhooked_at.is_none() {
            return Ok(());
        }
        self.mutate(|ledger| {
            if let Some(held) = ledger
                .reports
                .iter_mut()
                .find(|held| held.task_id == report.task_id)
            {
                held.unhooked_at = None;
            }
        })
    }

    /// The issue is no longer delegated to this app. Stop once, say so once,
    /// then drop the report so nothing writes to it again.
    ///
    /// A failed stop leaves the report in place so the next pass retries it; the
    /// notice is only posted after the work is actually stopped.
    async fn unhook(&self, host: &dyn Host, report: &IssueReport) {
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
                &report.tracker_session_id,
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

    /// Report new task history outward, advancing the cursor only after each
    /// event's action succeeded.
    ///
    /// The operator's acceptance is read from the task's own history, whether it
    /// came from a tracker reply or the local interface, and produces the one
    /// Done transition, final response, and unhook. An agent run ending is not an
    /// ending and never implies Done.
    async fn sync_report(&self, host: &dyn Host, report: &IssueReport) -> Result<(), String> {
        let events = host.events(report.task_id.clone()).await?;
        // A cursor past the end means the history it counted is gone, which is not a
        // reason to replay from the beginning: a replay re-writes statuses and
        // re-resolves threads the operator has since touched themselves.
        if report.reported_events > events.len() {
            self.set_cursor(&report.task_id, events.len())?;
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
            self.set_cursor(&report.task_id, index + 1)?;
        }
        if folded {
            self.replay_signals(host, report, &events).await?;
        }
        Ok(())
    }

    /// Give up on an event after it has been refused enough times.
    ///
    /// A tracker that will not accept one event — an issue that was deleted, a
    /// comment it says is gone — used to hold the cursor there forever: the same
    /// write every ten seconds, and nothing after it ever reported. Skipping is
    /// loud and bounded, which is the better failure.
    fn refused_event(
        &self,
        report: &IssueReport,
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
                    .reports
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
                .reports
                .iter_mut()
                .find(|held| held.task_id == report.task_id)
            {
                held.failed_at = None;
                held.failed_attempts = 0;
            }
        })?;
        self.set_cursor(&report.task_id, index + 1)
    }

    /// Report one event of a task's history.
    ///
    /// `Some(())` means the event was the task's ending, which closes the report.
    async fn report_event(
        &self,
        host: &dyn Host,
        report: &IssueReport,
        events: &[(i64, TaskHistoryEvent)],
        index: usize,
        unresolved: &HashSet<String>,
    ) -> Result<Option<()>, String> {
        let (_, event) = &events[index];
        match event {
            TaskHistoryEvent::AgentModelObserved { .. } => {
                if let Ok(Some(session_agent)) = host.session_agent(report.task_id.clone()).await
                    && let Err(error) = self
                        .set_session_summary(
                            &report.tracker_session_id,
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
                        &report.tracker_session_id,
                        &stable_id(&report.task_id, "paused"),
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
                    .update_session_plan(&report.tracker_session_id, steps)
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
                            &report.tracker_session_id,
                            &stable_id(&report.task_id, agent_run_id),
                            AgentActivityContent::Thought {
                                body: "Got your reply. Altitude is continuing the task.".into(),
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
                self.set_status(host, report, TrackerStatus::InProgress)
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
                        self.set_status(host, report, status).await?;
                    }
                }
            }
            TaskHistoryEvent::OperatorSignalResolved {
                signal_id,
                resolved: true,
                ..
            } => {
                self.set_signal_comment_state(report, signal_id, &events[..=index], true)
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
                self.set_signal_comment_state(report, signal_id, &events[..=index], false)
                    .await;
                if !self.is_settled(&report.issue_id)
                    && let Err(error) = self.set_status(host, report, TrackerStatus::InReview).await
                {
                    tracing::warn!(issue = %report.issue_id, %error, "could not say the task is waiting again");
                }
            }
            TaskHistoryEvent::OperatorSignalConfirmed { signal_id } => {
                self.set_signal_comment_state(report, signal_id, &events[..=index], true)
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

    /// The destination still owes the operator whatever is waiting, even though the
    /// rest of the history was already reported at the source.
    async fn replay_signals(
        &self,
        host: &dyn Host,
        report: &IssueReport,
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
                self.set_status(host, report, status).await?;
            }
        }
        self.set_cursor(&report.task_id, events.len())
    }

    /// The task moved to another machine: say so and drop the report without
    /// stopping the work or marking the issue Done.
    async fn finish_transferred(&self, report: &IssueReport) -> Result<(), String> {
        self.config
            .automation
            .create_activity(
                &report.tracker_session_id,
                &stable_id(&report.task_id, "transferred"),
                AgentActivityContent::Response {
                    body: "This task moved to another remote and keeps running there.".to_owned(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        self.finish_report(report)
    }

    /// Publish one signal on the tracker, and answer the status the issue should
    /// now have. The status is the caller's to write: it is the report's business
    /// whether the issue moves, not one comment's.
    async fn publish_signal(
        &self,
        report: &IssueReport,
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
                let body = format!("**○ Info:** {}\n\n{}", title.trim(), body.trim());
                self.config
                    .automation
                    .create_activity(
                        &report.tracker_session_id,
                        &stable_id(&report.task_id, signal_id),
                        AgentActivityContent::Thought { body },
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
                let body = format!("**? Needs you:** {}\n\n{}", title.trim(), body.trim());
                self.config
                    .automation
                    .comment_on_issue(&report.issue_id, &body, &comment_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(None)
            }
            TaskOperatorSignalKind::InReview => {
                let mut body =
                    format!("**◆ Ready to review:** {}\n\n{}", title.trim(), body.trim());
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

    /// Close or reopen the tracker's thread for one signal.
    ///
    /// A log has no thread to close; everything else follows what the operator did
    /// in the interface, so the tracker and this machine say the same thing about
    /// whether a question is still waiting.
    async fn set_signal_comment_state(
        &self,
        report: &IssueReport,
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
        let comment_id = stable_id(
            "comment",
            &format!("signal:{}:{}", report.task_id, signal_id),
        );
        let outcome = if resolved {
            self.config
                .automation
                .resolve_comment(&comment_id, None)
                .await
        } else {
            self.config.automation.reopen_comment(&comment_id).await
        };
        if let Err(error) = outcome {
            // A thread that was never published has nothing to close or reopen: a
            // signal resolved before this machine could report had no comment, and a
            // raise that is already answered is deliberately not published.
            if !error.to_string().contains("Entity not found") {
                tracing::warn!(
                    task = %report.task_id,
                    signal = signal_id,
                    resolved,
                    %error,
                    "could not follow a signal's thread in the tracker"
                );
            }
        }
    }

    async fn resolve_signal_comments(
        &self,
        report: &IssueReport,
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
            self.set_signal_comment_state(report, &signal.id, events, true)
                .await;
        }
    }

    /// Put a status on the issue, unless the issue already says it.
    ///
    /// Says nothing when there is nothing to say: a repeat is a write nobody asked
    /// for, and a tracker's history is read by people.
    async fn set_status(
        &self,
        host: &dyn Host,
        report: &IssueReport,
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
                .reports
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

    /// The operator moved the issue, so what this machine last said is no longer
    /// what it says. Forgetting it is what lets altitude answer them later.
    fn forget_status(&self, issue_id: &str) -> Result<(), String> {
        self.mutate(|ledger| {
            for held in ledger
                .reports
                .iter_mut()
                .filter(|held| held.issue_id == issue_id)
            {
                held.last_status = None;
            }
        })
    }

    /// Say the task is running again, unless the operator has already ended the
    /// issue: an issue nobody has moved since is the one this machine may write to.
    async fn say_task_is_running(&self, host: &dyn Host, report: &IssueReport) {
        if self.is_settled(&report.issue_id) {
            return;
        }
        if let Err(error) = self
            .set_status(host, report, TrackerStatus::InProgress)
            .await
        {
            tracing::warn!(issue = %report.issue_id, %error, "could not say the task is running again");
        }
    }

    /// Write the one ending an issue gets, and only once.
    ///
    /// A report replays its history, and a pass that stops before advancing its
    /// cursor replays the same acceptance: writing Done each time is how a machine
    /// fought an operator who had moved the issue themselves. The issue is recorded
    /// as settled when its ending is written, and a later acceptance for a task on
    /// the same issue says nothing new.
    async fn finish_accepted(
        &self,
        host: &dyn Host,
        report: &IssueReport,
        events: &[(i64, TaskHistoryEvent)],
    ) -> Result<(), String> {
        if !self.is_settled(&report.issue_id) {
            self.set_status(host, report, TrackerStatus::Done).await?;
            self.settle(&report.issue_id)?;
        }
        // The activity is what the session shows, not the issue's state: a failure
        // to say it must not hold the cursor on an ending that has been written.
        if let Err(error) = self
            .config
            .automation
            .create_activity(
                &report.tracker_session_id,
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

    async fn post_sync_guidance(&self, session_id: &str) -> Result<(), String> {
        let tracker = TrackerKind::Linear.label();
        let host = whoami::hostname().unwrap_or_else(|_| "this remote".to_owned());
        let body = format!(
            "**This remote isn't set up for delegated work yet.**\n\n\
             No one has added this remote to a project in Altitude, so it has no code, no \
             agent, and no settings to run this issue with.\n\n\
             On your computer:\n\n\
             1. Open Altitude, or install it if you don't have it.\n\
             2. In Settings → Remotes, add this remote: {host}\n\
             3. In Settings → Task tracker, sign in to {tracker}, so Altitude knows whose \
             work this is.\n\n\
             Altitude passes your setup to this remote in the background. Reply here once \
             that's done, and this issue gets picked up."
        );
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

    /// A remote could not start the work; say what to fix instead of retrying
    /// forever.
    async fn post_start_guidance(
        &self,
        session_id: &str,
        refusal: &super::StartRefusal,
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

    /// The client whose login answers for a tracker account.
    ///
    /// The login that arrived most recently wins, so a client that has just signed
    /// in answers for the account; a tie is broken by the client's own key, so the
    /// answer does not depend on directory order.
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

    /// Whether this client has synced a tracker account, which is what a report
    /// needs to be answerable.
    pub fn has_account(&self, owner: &str) -> bool {
        self.user_for_owner(owner).is_some()
    }

    fn user_for_owner(&self, owner: &str) -> Option<String> {
        logins(&self.config.operators_dir)
            .into_iter()
            .find(|login| login.owner == owner)
            .map(|login| login.account.user_id)
    }

    /// Record that an issue's ending is written, whoever wrote it.
    fn settle(&self, issue_id: &str) -> Result<(), String> {
        self.mutate(|ledger| {
            if !ledger.settled_issues.iter().any(|id| id == issue_id) {
                ledger.settled_issues.push(issue_id.to_owned());
            }
        })
    }

    /// Whether this issue's ending has been written, which is what stops a machine
    /// from contradicting an operator who has moved the issue since.
    fn is_settled(&self, issue_id: &str) -> bool {
        self.lock()
            .map(|ledger| ledger.settled_issues.iter().any(|id| id == issue_id))
            .unwrap_or(false)
    }

    fn report_for_task(&self, task_id: &str) -> Option<IssueReport> {
        let ledger = self.lock().ok()?;
        ledger
            .reports
            .iter()
            .find(|report| report.task_id == task_id)
            .cloned()
    }

    fn report_for_session(&self, session_id: &str) -> Option<IssueReport> {
        let ledger = self.lock().ok()?;
        ledger
            .reports
            .iter()
            .find(|report| report.tracker_session_id == session_id)
            .cloned()
    }

    fn report_for_issue(&self, issue_id: &str) -> Option<IssueReport> {
        let ledger = self.lock().ok()?;
        let mut held: Vec<&IssueReport> = ledger
            .reports
            .iter()
            .filter(|report| report.issue_id == issue_id || report.issue_identifier == issue_id)
            .collect();
        // A finished report is history; a task still being told about wins, so a
        // second task on the same issue is reported rather than quietly skipped.
        held.sort_by_key(|report| report.finished);
        held.first().map(|report| (*report).clone())
    }

    fn reports(&self) -> Vec<IssueReport> {
        self.lock()
            .map(|ledger| ledger.reports.clone())
            .unwrap_or_default()
    }

    /// Record one task's reporting, replacing what this task had before.
    ///
    /// Keyed by task, not by issue: a finished task keeps its report so it is never
    /// adopted again, and a second task started from the same issue gets one of its
    /// own.
    fn upsert_report(&self, report: IssueReport) -> Result<(), String> {
        self.mutate(|ledger| {
            ledger
                .reports
                .retain(|existing| existing.task_id != report.task_id);
            ledger.reports.push(report);
        })
    }

    /// Note that a task's life is fully told, and stop writing about it.
    ///
    /// The report stays in the ledger, finished: the task is still here, and a
    /// report that was thrown away looks exactly like one that was never made, so
    /// the next pass would open another session and tell the same story again.
    fn finish_report(&self, report: &IssueReport) -> Result<(), String> {
        self.mutate(|ledger| {
            for existing in &mut ledger.reports {
                if existing.task_id == report.task_id {
                    existing.finished = true;
                }
            }
        })
    }

    fn retire_report(&self, report: &IssueReport) -> Result<(), String> {
        let now = now_ms();
        self.mutate(|ledger| {
            ledger
                .reports
                .retain(|existing| existing.task_id != report.task_id);
            ledger
                .terminated_sessions
                .retain(|session| session.session_id != report.tracker_session_id);
            ledger.terminated_sessions.push(TerminatedSession {
                session_id: report.tracker_session_id.clone(),
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

    fn set_cursor(&self, task_id: &str, cursor: usize) -> Result<(), String> {
        self.mutate(|ledger| {
            if let Some(report) = ledger.reports.iter_mut().find(|r| r.task_id == task_id) {
                report.reported_events = cursor;
            }
        })
    }

    /// A reference used only to address an issue by id.
    ///
    /// The API reads and writes by id and checks the workspace from the issue it
    /// returns, so the empty identifier and url are never read.
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

/// What a signed webhook turns into, or why it turns into nothing.
///
/// The reasons are logged where the outcome is returned, so the operator reading
/// the log can tell a misconfiguration (a warning) from chatter (debug). A
/// rejection carries its detail because the only person who can fix a body this
/// machine cannot read is the one reading that line; an ignore is a fixed phrase
/// about a delivery that was handled correctly.
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueEvent {
    organization_id: String,
    webhook_timestamp: f64,
    created_at: String,
    actor: IssueActor,
    data: IssueData,
    #[serde(default)]
    updated_from: Option<IssueUpdatedFrom>,
}

#[derive(Deserialize)]
struct IssueActor {
    id: String,
    #[serde(rename = "type")]
    actor_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueData {
    id: String,
    #[serde(default)]
    state_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueUpdatedFrom {
    #[serde(default)]
    state_id: Option<String>,
}

fn parse_issue_event(body: &[u8]) -> Result<IssueEvent, serde_json::Error> {
    serde_json::from_slice(body)
}

impl IssueEvent {
    fn is_fresh(&self) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        (now.as_millis() as f64 - self.webhook_timestamp).abs()
            <= WEBHOOK_MAX_AGE.as_millis() as f64
    }

    /// Only a state change made by a human becomes work; anything else is not
    /// this coordinator's business.
    fn into_stored(self) -> Option<StoredEvent> {
        if self.actor.actor_type != "user" {
            return None;
        }
        let state_id = self.data.state_id?;
        let previous_state_id = self.updated_from?.state_id?;
        if previous_state_id == state_id {
            return None;
        }
        let occurred_at_ms = chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .ok()?
            .timestamp_millis();
        Some(StoredEvent::IssueStateChanged(IssueStateChanged {
            issue_id: self.data.id,
            state_id,
            actor_id: self.actor.id,
            occurred_at_ms,
        }))
    }
}

/// An `AppUserNotification` webhook, as far as the coordinator trusts it.
///
/// Only notifications that carry a comment are turned into input. A status or
/// assignment notification is lifecycle, not a message to the agent.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppNotificationEvent {
    organization_id: String,
    app_user_id: String,
    #[serde(default)]
    oauth_client_id: Option<String>,
    webhook_timestamp: f64,
    action: String,
    notification: AppNotification,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppNotification {
    #[serde(rename = "type")]
    notification_type: String,
    #[serde(default)]
    actor_id: Option<String>,
    #[serde(default)]
    comment_id: Option<String>,
    #[serde(default)]
    issue_id: Option<String>,
    #[serde(default)]
    document_id: Option<String>,
    #[serde(default)]
    comment: Option<NotificationComment>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotificationComment {
    #[serde(default)]
    id: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    issue_id: Option<String>,
}

fn parse_app_notification(body: &[u8]) -> Result<AppNotificationEvent, serde_json::Error> {
    serde_json::from_slice(body)
}

impl AppNotificationEvent {
    fn is_fresh(&self) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        (now.as_millis() as f64 - self.webhook_timestamp).abs()
            <= WEBHOOK_MAX_AGE.as_millis() as f64
    }

    /// Turn the notification into the comment input it carries, if it carries
    /// one. Everything else is left to the lifecycle handlers.
    fn into_stored(self) -> Option<StoredEvent> {
        let is_mention = matches!(
            self.action.as_str(),
            "issueCommentMention" | "documentCommentMention"
        ) || self.notification.notification_type.ends_with("Mention");
        let is_new_comment = matches!(
            self.action.as_str(),
            "issueNewComment" | "documentNewComment"
        );
        if !is_mention && !is_new_comment {
            return None;
        }
        let comment = self.notification.comment.unwrap_or_default();
        let comment_id = self
            .notification
            .comment_id
            .or_else(|| (!comment.id.is_empty()).then_some(comment.id))?;
        let actor_id = self
            .notification
            .actor_id
            .or(comment.user_id)
            .filter(|id| !id.is_empty())?;
        let issue_id = self.notification.issue_id.or(comment.issue_id);
        Some(StoredEvent::Comment(CommentInput {
            comment_id,
            issue_id,
            document_id: self.notification.document_id,
            actor_id,
            is_mention,
            body: comment.body,
        }))
    }
}

// ------------------------------------------------------------------- storage

/// A tracker login a client has pushed to this machine.
pub struct Login {
    pub owner: String,
    pub account: TrackerAccount,
    /// When the login last arrived here, which is how this machine tells two
    /// clients signed in as the same person apart.
    pub arrived: SystemTime,
}

/// The tracker logins this machine holds, read from the profiles that arrived.
///
/// The login on disk is the fact. A record of it beside the login is a second copy,
/// and a second copy can be missing while the fact is there — which is exactly how
/// one machine spent a day reporting nothing while holding the login all along.
pub fn logins(operators_dir: &Path) -> Vec<Login> {
    let Ok(entries) = fs::read_dir(operators_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .flat_map(|entry| {
            let owner = entry.file_name().to_string_lossy().into_owned();
            TrackerKind::ALL
                .iter()
                .flat_map(move |provider| logins_of(&entry.path(), &owner, *provider))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// One client's logins of one tracker, oldest name first.
///
/// A login its client has yielded to another is not a claim and is left out: the
/// credential is still theirs to use, but their work here is no longer attributed
/// to that account.
pub fn logins_of(operator_dir: &Path, owner: &str, provider: TrackerKind) -> Vec<Login> {
    if yielded_providers(operator_dir).contains(&provider) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(operator_dir.join(provider.profile())) else {
        return Vec::new();
    };
    let mut logins: Vec<Login> = entries
        .flatten()
        .filter_map(|login| {
            let identity = login.path().join("profile").join(ACCOUNT_FILE);
            let arrived = fs::metadata(&identity).ok()?.modified().ok()?;
            let account = fs::read(&identity)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<TrackerAccount>(&bytes).ok())
                .filter(|account| account.provider == provider)?;
            Some(Login {
                owner: owner.to_owned(),
                account,
                arrived,
            })
        })
        .collect();
    logins.sort_by(|left, right| left.account.user_id.cmp(&right.account.user_id));
    logins
}

/// Note that this client no longer claims a tracker, because another client took
/// it over.
///
/// What the client pushed stays where it is: the login is the same account, and
/// which client a task is attributed to is the question here, not who holds the
/// credentials.
pub fn yield_account(operator_dir: &Path, provider: TrackerKind) -> io::Result<()> {
    set_yielded(operator_dir, provider, true)
}

/// A client that pushes a login again claims it again.
pub fn claim_account(operator_dir: &Path, provider: TrackerKind) -> io::Result<()> {
    set_yielded(operator_dir, provider, false)
}

#[derive(Default, Serialize, Deserialize)]
struct Yielded {
    #[serde(default)]
    providers: Vec<TrackerKind>,
}

fn yielded_path(operator_dir: &Path) -> PathBuf {
    operator_dir.join("yielded.json")
}

fn yielded_providers(operator_dir: &Path) -> Vec<TrackerKind> {
    fs::read_to_string(yielded_path(operator_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<Yielded>(&text).ok())
        .map(|yielded| yielded.providers)
        .unwrap_or_default()
}

fn set_yielded(operator_dir: &Path, provider: TrackerKind, yielded: bool) -> io::Result<()> {
    let path = yielded_path(operator_dir);
    let mut held = yielded_providers(operator_dir);
    let had = held.contains(&provider);
    if yielded == had {
        return Ok(());
    }
    if yielded {
        held.push(provider);
    } else {
        held.retain(|held| *held != provider);
    }
    if held.is_empty() {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    let text =
        serde_json::to_string_pretty(&Yielded { providers: held }).map_err(io::Error::other)?;
    super::identity::write_private(&path, &text).map_err(io::Error::other)
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    deliveries: Vec<Delivery>,
    #[serde(default)]
    reports: Vec<IssueReport>,
    #[serde(default)]
    terminated_sessions: Vec<TerminatedSession>,
    /// The issues whose ending has been written. One issue gets one ending, however
    /// many passes replay how it got there.
    #[serde(default)]
    settled_issues: Vec<String>,
}

impl Ledger {
    fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                format!(
                    "could not read the tracker ledger at {}: {error}",
                    path.display()
                )
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!(
                "could not read the tracker ledger at {}: {error}",
                path.display()
            )),
        }
    }

    fn prune(&mut self, now: i64) {
        self.prune_done(now);
        self.prune_terminated(now);
    }

    /// Keep pending deliveries in arrival order and retain only recent finished
    /// ones, so a redelivery is still deduped without the file growing forever.
    fn prune_done(&mut self, now: i64) {
        let cutoff = now - DONE_RETENTION.as_millis() as i64;
        let mut pending = Vec::new();
        let mut done = Vec::new();
        for delivery in self.deliveries.drain(..) {
            if delivery.done {
                if delivery.received_at_ms >= cutoff {
                    done.push(delivery);
                }
            } else {
                pending.push(delivery);
            }
        }
        done.sort_by_key(|delivery| Reverse(delivery.received_at_ms));
        done.truncate(MAX_DONE);
        pending.extend(done);
        self.deliveries = pending;
    }

    fn prune_terminated(&mut self, now: i64) {
        let cutoff = now - TERMINATED_RETENTION.as_millis() as i64;
        self.terminated_sessions
            .retain(|session| session.terminated_at_ms >= cutoff);
        if self.terminated_sessions.len() > MAX_TERMINATED {
            let excess = self.terminated_sessions.len() - MAX_TERMINATED;
            self.terminated_sessions.drain(..excess);
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Delivery {
    key: String,
    received_at_ms: i64,
    next_attempt_at_ms: i64,
    done: bool,
    event: StoredEvent,
}

#[derive(Clone, Serialize, Deserialize)]
/// One task being told to one tracker issue.
///
/// The task's own history is what happened; this is how much of it has been told,
/// through which tracker session, and whose login is telling it. It is written
/// once, when the task is created or imported, and dropped when the task ends.
struct IssueReport {
    /// The issue in the tracker's own terms, which is what the API takes.
    issue_id: String,
    /// The name people know it by, `ALT-12`. Reported through because it is what a
    /// reference made from a search carries, and a report has to answer to either.
    #[serde(default)]
    issue_identifier: String,
    /// The session this machine opened on that issue, so a second one is never
    /// opened for the same work.
    tracker_session_id: String,
    /// The client whose pushed login reports this task: a report speaks as the
    /// person the work belongs to, not as the machine.
    owner: String,
    tracker_user_id: String,
    task_id: String,
    /// How many events of the task's history have been reported. Reporting resumes
    /// here, so a failure repeats one event rather than the whole history.
    #[serde(default)]
    reported_events: usize,
    #[serde(default)]
    origin: ReportOrigin,
    /// Whether this task has ended and there is nothing left to tell. A finished
    /// report stays: dropping it would make the task look unreported, and a task
    /// is reported once.
    #[serde(default)]
    finished: bool,
    /// The event a pass is stuck on, and how many passes have refused it. An event
    /// the tracker will not take is skipped rather than pinning the report forever.
    #[serde(default)]
    failed_at: Option<usize>,
    #[serde(default)]
    failed_attempts: usize,
    /// The status this machine last put on the issue. An agent run that starts
    /// writes In Progress, and a task with ten runs must not write it ten times:
    /// the tracker's history is not a log of altitude's clock.
    #[serde(default)]
    last_status: Option<TrackerStatus>,
    /// When the issue was first seen not delegated to this app. A lapse is waited
    /// out before it ends the task, so a delegation toggled twice in a minute
    /// leaves the work alone.
    #[serde(default)]
    unhooked_at: Option<i64>,
}

/// Who started the work, which decides whether the report outlives the tracker's
/// own delegation of the issue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReportOrigin {
    /// The tracker delegated the issue to this app, so an issue that stops being
    /// delegated stops being reported.
    #[default]
    Delegated,
    /// A client started the task on the issue, so this machine only reports it:
    /// the issue need not stay delegated and a native stop is still accepted.
    StartedByClient,
}

#[derive(Clone, Serialize, Deserialize)]
struct TerminatedSession {
    session_id: String,
    terminated_at_ms: i64,
}

/// What a signed webhook left behind after the request is over.
///
/// The raw body is not kept: the webhook user carries an email, and a delivery
/// ledger is not the place for it. Only ids and the operator's own words remain.
#[derive(Clone, Serialize, Deserialize)]
enum StoredEvent {
    Session(Box<SessionEvent>),
    IssueStateChanged(IssueStateChanged),
    Comment(CommentInput),
}

impl StoredEvent {
    /// What arrived, by id, for a log line that says what happened without
    /// repeating what the delivery said.
    fn describe(&self) -> String {
        match self {
            StoredEvent::Session(event) => {
                format!("session {} · {}", event.session_id, event.action)
            }
            StoredEvent::IssueStateChanged(change) => {
                format!("issue {} · state {}", change.issue_id, change.state_id)
            }
            StoredEvent::Comment(comment) => {
                format!(
                    "comment {} · {}",
                    comment.comment_id,
                    if comment.is_mention {
                        "mention"
                    } else {
                        "comment"
                    }
                )
            }
        }
    }

    fn dedupe_key(&self) -> String {
        match self {
            StoredEvent::Session(session) => session.dedupe_key(),
            StoredEvent::IssueStateChanged(change) => format!(
                "issue:{}:{}:{}:{}",
                change.issue_id, change.state_id, change.actor_id, change.occurred_at_ms
            ),
            StoredEvent::Comment(comment) => format!("comment:{}", comment.comment_id),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SessionEvent {
    action: String,
    session_id: String,
    #[serde(default)]
    issue_id: Option<String>,
    #[serde(default)]
    comment_id: Option<String>,
    #[serde(default)]
    creator_id: Option<String>,
    #[serde(default)]
    creator_name: Option<String>,
    #[serde(default)]
    prompt_author_id: Option<String>,
    #[serde(default)]
    prompt_body: Option<String>,
    #[serde(default)]
    signal: Option<String>,
    #[serde(default)]
    source_comment_id: Option<String>,
    /// The signed activity id, which is a prompt's own identity: a redelivery is
    /// the same activity whatever the unsigned delivery header says.
    #[serde(default)]
    activity_id: Option<String>,
    /// When the signed webhook happened, for ordering a prompt against the
    /// signal it means to answer.
    #[serde(default)]
    occurred_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct IssueStateChanged {
    issue_id: String,
    state_id: String,
    actor_id: String,
    occurred_at_ms: i64,
}

/// A comment notification reduced to what admission needs.
///
/// The body is kept because a wake hands the operator's words to the task. It
/// is never written to a log line.
#[derive(Clone, Serialize, Deserialize)]
struct CommentInput {
    comment_id: String,
    #[serde(default)]
    issue_id: Option<String>,
    #[serde(default)]
    document_id: Option<String>,
    actor_id: String,
    is_mention: bool,
    body: String,
}

impl SessionEvent {
    /// A stop is the operator's own word or Linear's stop signal, never the
    /// session's status.
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

    /// What makes this the same signed event as one already recorded.
    ///
    /// A creation is the session's action; a prompt is the activity itself, so
    /// two prompts with the same words are still two answers; a stop names the
    /// activity that carried it.
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

/// A UUID that is stable for one tracker object, so a lost response can be
/// retried without creating a second activity, comment, or reaction.
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

/// Whether an operator's reply is a clear acceptance of a wrap-up request.
///
/// A reply that is not an acceptance is sent back to the task as further
/// instructions, which is the safe default.
fn is_acceptance(body: &str) -> bool {
    let normalized = body.trim().to_ascii_lowercase();
    [
        "accept",
        "accepted",
        "approve",
        "approved",
        "lgtm",
        "done",
        "complete",
        "completed",
        "ship it",
        "looks good",
    ]
    .contains(&normalized.as_str())
}

/// Whether this history crossed machines, which is what makes replaying it
/// would-be duplicate reporting rather than news.
///
/// The arrival itself records a staged transfer, so the destination sees one
/// even though the source's own transfer events were left behind.
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
