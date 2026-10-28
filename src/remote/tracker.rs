//! The remote's Linear coordinator: signed webhooks in, task host out.
//!
//! A serve role has no person at the keyboard. Work arrives as a Linear
//! AgentSessionEvent, an Issue status change, or a comment notification, is
//! durably recorded before the webhook is acknowledged, and is then routed to
//! the task host that owns the capsule. Comments pass through
//! [`super::admission`] before anything reacts or wakes. This module owns only
//! the mapping between an external issue/session and a local task, a peer, and a
//! tracker user; the task's own state is always read back from the host, never
//! cached here.
//!
//! The coordinator calls the host through [`Host`], a small callback surface the
//! serve role implements over its task event loop. Keeping the two apart is what
//! lets the execution core stay the source of operational truth.

use std::{
    cmp::Reverse,
    collections::HashSet,
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
        IssueReference, IssueSnapshot, TrackerAccount, TrackerKind, TrackerStatus,
        automation::{
            AgentActivityContent, AgentSessionEvent, LinearAutomation, WebhookError,
            parse_agent_session_event, verify_signature,
        },
    },
};

use super::{
    admission::{self, Actor, Admission, InputFacts},
    identity::write_private,
};

/// One webhook request, and the one resource this coordinator handles.
const WEBHOOK_ROUTE: &str = "/altitude/linear/webhook";
const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;
const SIGNATURE_HEADER: &str = "linear-signature";

/// How often the coordinator reports task life. A webhook wakes it sooner.
const TICK: Duration = Duration::from_secs(10);
/// A failed delivery is retried at a fixed pace: the work is already durable on
/// disk, so a tracker outage should not produce a request storm.
const RETRY: Duration = Duration::from_secs(10);
/// Completed deliveries and terminated sessions are kept long enough that a
/// redelivery or a replay is still recognised.
const DONE_RETENTION: Duration = Duration::from_secs(600);
const TERMINATED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DONE: usize = 512;
const MAX_TERMINATED: usize = 1024;
/// A Linear webhook is only trusted while it is this fresh.
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
    /// This machine's name, as the session summary reads it.
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
    /// Cancel a task and dispose of its capsule. Never an acceptance.
    fn stop(&self, task_id: String) -> HostFuture<'_, ()>;
    /// The task's currently unresolved operator signals.
    fn unresolved_signals(&self, task_id: String) -> HostFuture<'_, Vec<TaskOperatorSignalThread>>;
    /// The task's history, oldest first.
    fn events(&self, task_id: String) -> HostFuture<'_, Vec<(i64, TaskHistoryEvent)>>;
    /// Add the operator's instructions to the task, waking it if it is not running.
    fn instruct(&self, task_id: String, instruction: String) -> HostFuture<'_, ()>;
    /// Reply in a thread, as the tracker's operator did.
    fn reply(&self, task_id: String, signal_id: String, answer: String) -> HostFuture<'_, ()>;
    /// Accept a wrap-up request, the only way a task ends.
    fn accept(&self, task_id: String, signal_id: String) -> HostFuture<'_, ()>;
    /// Whether the task is still in play, so a removed task stops being reported.
    fn is_active(&self, task_id: String) -> HostFuture<'_, bool>;
    /// The harness and model a task runs, for the session title.
    fn session_agent(&self, task_id: String) -> HostFuture<'_, Option<String>>;
}

pub struct RemoteTracker {
    config: RemoteTrackerConfig,
    ledger: Mutex<Ledger>,
    /// Woken when a webhook is recorded, so the operator does not wait a tick.
    notify: Notify,
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
    /// The peer must have synced a tracker account: a route needs a tracker user
    /// who can answer it, and a peer that has pushed none has nobody. The session
    /// is made through the public Agent API so an issue this app was never
    /// delegated can still be reported on, and the human assignee is left alone. A
    /// route that already holds the task or the issue is left as it is, so a retry
    /// cannot sponsor a second session once one has been recorded.
    pub async fn pilot_task(
        &self,
        peer: EndpointId,
        task_id: String,
        issue: IssueReference,
        session_agent: Option<String>,
    ) -> Result<(), String> {
        let Some(tracker_user_id) = self.user_for_peer(&peer.to_string()) else {
            return Err("sync a Linear account to this remote before delegating issue work".into());
        };
        if self.route_for_task(&task_id).is_some() || self.route_for_issue(&issue.id).is_some() {
            return Ok(());
        }
        let session_id = self
            .config
            .automation
            .create_session_on_issue(&issue.id)
            .await
            .map_err(|error| error.to_string())?;
        self.upsert_route(Route {
            issue_id: issue.id,
            session_id: session_id.clone(),
            peer: peer.to_string(),
            tracker_user_id: tracker_user_id.clone(),
            task_id,
            cursor: 0,
            origin: RouteOrigin::Piloted,
        })?;
        if let Err(error) = self
            .set_session_summary(&session_id, &tracker_user_id, session_agent.as_deref())
            .await
        {
            tracing::warn!(session = %session_id, %error, "could not name a piloted session");
        }
        Ok(())
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
            self.poll_routes(host.as_ref()).await;
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

    /// A stop is a cancellation, never an acceptance or a status restore.
    ///
    /// Only the delegation operator may stop the work; a teammate's stop is not
    /// the operator's judgment and is ignored. The route is dropped only after
    /// the stop and its notice have both succeeded, so nothing writes to it again.
    async fn handle_stop(&self, event: &SessionEvent, host: &dyn Host) -> Result<(), String> {
        let Some(route) = self.route_for_session(&event.session_id) else {
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
        if !event.is_from(&route.tracker_user_id) {
            return Ok(());
        }
        host.stop(route.task_id.clone()).await?;
        self.config
            .automation
            .create_activity(
                &route.session_id,
                &stable_id(&route.task_id, "stopped"),
                AgentActivityContent::Response {
                    body: "Stopped. Altitude ended this task and deleted its sandbox.".to_owned(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        self.retire_route(&route)
    }

    /// A new Agent Session is a delegation, or a redelegation of active work.
    async fn handle_created(&self, event: &SessionEvent, host: &dyn Host) -> Result<(), String> {
        let Some(issue_id) = event.issue_id.as_deref() else {
            return Ok(());
        };
        if self.is_terminated(&event.session_id)
            || self.route_for_session(&event.session_id).is_some()
        {
            return Ok(());
        }
        // The session's creator is the delegation author. An automation- or
        // agent-started session has no responsible human, so there is nobody to
        // route through and no assignee fallback is invented. An app-created id
        // cannot be guessed back to a person, and an issue already being worked
        // on is not news, so only an unknown issue is worth educating.
        let Some(author) = event
            .creator_id
            .as_deref()
            .filter(|id| *id != self.config.automation.app_user_id())
        else {
            if self.route_for_issue(issue_id).is_none() {
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
        if let Some(route) = self.route_for_issue(issue_id)
            && is_mention
        {
            if let Some(source_comment_id) = event
                .source_comment_id
                .as_ref()
                .or(event.comment_id.as_ref())
            {
                self.react_eyes(source_comment_id).await;
            }
            if author == route.tracker_user_id {
                let body = event.prompt_body.clone().unwrap_or_default();
                if !body.trim().is_empty() {
                    host.instruct(
                        route.task_id,
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
                            &stable_id(&event.session_id, "routed"),
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
            let body = self.boundary_reply(&route.tracker_user_id).await;
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
        if self.route_for_session(session_id).is_some() {
            return Ok(());
        }

        // A different session on an issue that already has work is a
        // redelegation: stop the old task before the new one starts, so two
        // capsules never own the same issue.
        let redelegation = if let Some(existing) = self.route_for_issue(issue_id) {
            host.stop(existing.task_id.clone()).await?;
            self.config.automation.create_activity(&existing.session_id, &stable_id(&existing.task_id, "redelegated"), AgentActivityContent::Response {
                body: "A newer delegation replaced this one, so Altitude stopped this task and deleted its sandbox.".into(),
            }).await.map_err(|error| error.to_string())?;
            self.retire_route(&existing)?;
            true
        } else {
            false
        };

        let Some(peer) = self.peer_for_user(author) else {
            self.post_sync_guidance(session_id).await?;
            return Ok(());
        };
        let peer: EndpointId = peer
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
        self.upsert_route(Route {
            issue_id: issue_id.to_owned(),
            session_id: session_id.to_owned(),
            peer: peer.to_string(),
            tracker_user_id: author.to_owned(),
            task_id: task_id.clone(),
            cursor: 0,
            origin: RouteOrigin::Delegated,
        })?;
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

    /// A prompt is an operator's answer to whatever the task is waiting on.
    async fn handle_prompted(&self, event: &SessionEvent, host: &dyn Host) -> Result<(), String> {
        let Some(answer) = event.prompt_body.as_deref() else {
            return Ok(());
        };
        let Some(route) = self.route_for_session(&event.session_id) else {
            // No route yet: an operator's prompt on an issue session is a
            // delegation, which is how a session opened from the issue starts the
            // work. It is also the retry path after a failed start.
            if let Some(author) = event.prompt_author_id.as_deref()
                && let Some(issue_id) = event.issue_id.as_deref()
                && event.creator_id.as_deref() == Some(author)
                && self.peer_for_user(author).is_some()
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
        if !event.is_from(&route.tracker_user_id) {
            let body = self.boundary_reply(&route.tracker_user_id).await;
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
        let signals = host.unresolved_signals(route.task_id.clone()).await?;
        // A prompt answers only what was already waiting when it was written. A
        // prompt that predates a signal is stale, and letting it resolve the
        // newer question would answer something the operator never saw.
        let waiting: Vec<&TaskOperatorSignalThread> = signals
            .iter()
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
                .instruct(route.task_id.clone(), answer.to_owned())
                .await;
        };
        if signal.kind == TaskOperatorSignalKind::InReview && is_acceptance(answer) {
            host.accept(route.task_id.clone(), signal.id.clone())
                .await?;
            let events = host.events(route.task_id.clone()).await?;
            return self.finish_accepted(&route, &events).await;
        }
        host.reply(route.task_id.clone(), signal.id.clone(), answer.to_owned())
            .await
    }

    /// A user changed an issue's workflow state.
    ///
    /// Only the delegation operator's own change is the operator's judgment, and
    /// only a state the native API confirms is completed counts. A teammate's
    /// status change is ignored, as is a poll that merely observes Done.
    async fn handle_issue_state(
        &self,
        issue_id: &str,
        state_id: &str,
        actor_id: &str,
        occurred_at_ms: i64,
        host: &dyn Host,
    ) -> Result<(), String> {
        let Some(route) = self.route_for_issue(issue_id) else {
            return Ok(());
        };
        if route.tracker_user_id.as_str() != actor_id {
            return Ok(());
        }
        let delegation = self
            .config
            .automation
            .issue_delegation(issue_id)
            .await
            .map_err(|error| error.to_string())?;
        if delegation.state.id != state_id || delegation.state.state_type != "completed" {
            return Ok(());
        }
        let history = host.events(route.task_id.clone()).await?;
        let was_waiting = history
            .iter()
            .rev()
            .filter(|(time, _)| *time <= occurred_at_ms)
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
            return Ok(());
        }
        let signals = host.unresolved_signals(route.task_id.clone()).await?;
        let Some(review) = signals
            .iter()
            .find(|signal| signal.kind == TaskOperatorSignalKind::InReview)
        else {
            return Ok(());
        };
        // The review must already have been waiting when the operator marked the
        // issue done; a later one belongs to a later cycle.
        if review.last_raised_at_ms > occurred_at_ms {
            return Ok(());
        }
        host.accept(route.task_id.clone(), review.id.clone())
            .await?;
        let events = host.events(route.task_id.clone()).await?;
        self.finish_accepted(&route, &events).await
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
        let Some(route) = self.route_for_issue(&issue_id) else {
            return Ok(());
        };
        if !host.is_active(route.task_id.clone()).await? {
            return Ok(());
        }
        let app_user_id = self.config.automation.app_user_id();
        let actor = if event.actor_id == route.tracker_user_id {
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
                .any(|id| id.as_str() != route.tracker_user_id && id.as_str() != app_user_id);
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
            has_other_humans,
        };
        let decision = admission::decide(&facts);
        tracing::info!(
            comment = %event.comment_id,
            root = %site.root_comment_id,
            site_comment = %site.comment_id,
            task = %route.task_id,
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
                host.instruct(route.task_id, instruction).await
            }
            Admission::Redirect => {
                self.react_eyes(&event.comment_id).await;
                let body = self.boundary_reply(&route.tracker_user_id).await;
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

    /// A deterministic acknowledgement, so a retry cannot add a second reaction.
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

    /// `altitude / operator@host / harness`, the session's public title.
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
    /// the state. It is only ignored, so it never blocks the other routes.
    async fn poll_routes(&self, host: &dyn Host) {
        for route in self.routes() {
            // A task the operator removed is no longer work: stop writing about
            // it rather than reporting a history that will never advance. A task
            // that left because it moved elsewhere still gets one goodbye, and
            // neither ending stops the work or marks the issue Done.
            match host.is_active(route.task_id.clone()).await {
                Ok(true) => {}
                Ok(false) => {
                    let moved = host
                        .events(route.task_id.clone())
                        .await
                        .unwrap_or_default()
                        .iter()
                        .any(|(_, event)| {
                            matches!(event, TaskHistoryEvent::TransferCommitted { .. })
                        });
                    let outcome = if moved {
                        self.finish_transferred(&route).await
                    } else {
                        self.retire_route(&route)
                    };
                    if let Err(error) = outcome {
                        tracing::warn!(task = %route.task_id, %error, "could not drop a removed task's route");
                    }
                    continue;
                }
                Err(error) => {
                    tracing::warn!(task = %route.task_id, %error, "could not tell whether a task is still here");
                    continue;
                }
            }
            // A piloted route was not delegated to the app, so it does not end
            // when the issue stops being delegated; it still accepts a native
            // stop and the operator's own control.
            if route.origin == RouteOrigin::Delegated {
                let delegation = match self
                    .config
                    .automation
                    .issue_delegation(&route.issue_id)
                    .await
                {
                    Ok(delegation) => delegation,
                    Err(error) => {
                        tracing::warn!(issue = %route.issue_id, %error, "could not read the issue's delegation");
                        continue;
                    }
                };
                if delegation.delegate_id.as_deref() != Some(self.config.automation.app_user_id()) {
                    self.unhook(host, &route).await;
                    continue;
                }
            }
            if let Err(error) = self.sync_route(host, &route).await {
                tracing::warn!(task = %route.task_id, %error, "task updates remain queued");
            }
        }
    }

    /// The issue is no longer delegated to this app. Stop once, say so once,
    /// then drop the route so nothing writes to it again.
    ///
    /// A failed stop leaves the route in place so the next pass retries it; the
    /// notice is only posted after the work is actually stopped.
    async fn unhook(&self, host: &dyn Host, route: &Route) {
        if host.stop(route.task_id.clone()).await.is_err() {
            return;
        }
        if let Err(error) = self
            .config
            .automation
            .create_activity(
                &route.session_id,
                &stable_id(&route.task_id, "unhooked"),
                AgentActivityContent::Response {
                    body: "This issue isn't delegated to Altitude anymore, so the task was                            stopped."
                        .to_owned(),
                },
            )
            .await
        {
            tracing::warn!(task = %route.task_id, %error, "could not say why a task was unhooked");
            return;
        }
        if let Err(error) = self.retire_route(route) {
            tracing::warn!(task = %route.task_id, %error, "could not drop an unhooked route");
        }
    }

    /// Report new task history outward, advancing the cursor only after each
    /// event's action succeeded.
    ///
    /// The operator's acceptance is read from the task's own history, whether it
    /// came from a tracker reply or the local interface, and produces the one
    /// Done transition, final response, and unhook. An agent run ending is not an
    /// ending and never implies Done.
    async fn sync_route(&self, host: &dyn Host, route: &Route) -> Result<(), String> {
        let events = host.events(route.task_id.clone()).await?;
        if route.cursor > events.len() {
            self.set_cursor(&route.issue_id, 0)?;
            return Ok(());
        }
        // Signals still open are the only ones worth publishing; one already
        // answered or confirmed must not be raised again on the next pass.
        let unresolved: HashSet<String> = host
            .unresolved_signals(route.task_id.clone())
            .await?
            .into_iter()
            .map(|signal| signal.id)
            .collect();
        // A route that only began after a transfer must not replay the source's
        // whole life as new activity. Folding leaves the projected state below.
        let folded = route.cursor == 0 && was_transferred(&events);
        let start = if folded { events.len() } else { route.cursor };
        for (index, (_, event)) in events.iter().enumerate().skip(start) {
            match event {
                TaskHistoryEvent::AgentModelObserved { .. } => {
                    if let Ok(Some(session_agent)) = host.session_agent(route.task_id.clone()).await
                        && let Err(error) = self
                            .set_session_summary(
                                &route.session_id,
                                &route.tracker_user_id,
                                Some(&session_agent),
                            )
                            .await
                    {
                        tracing::warn!(task = %route.task_id, %error, "could not update the session model label");
                    }
                }
                TaskHistoryEvent::PlanUpdated { steps } => {
                    self.config
                        .automation
                        .update_session_plan(&route.session_id, steps)
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
                                &route.session_id,
                                &stable_id(&route.task_id, agent_run_id),
                                AgentActivityContent::Thought {
                                    body: "Got your reply. Altitude is continuing the task.".into(),
                                },
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    self.config
                        .automation
                        .status(
                            &self.issue_reference(&route.issue_id),
                            TrackerStatus::InProgress,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
                TaskHistoryEvent::OperatorSignalRaised { signal } => {
                    if unresolved.contains(&signal.id) {
                        let contributions = contribution_details(&events[..=index]);
                        self.publish_signal(
                            route,
                            &signal.id,
                            signal.kind,
                            &signal.title,
                            &signal.body,
                            &contributions,
                        )
                        .await?;
                    }
                }
                TaskHistoryEvent::OperatorSignalResolved {
                    signal_id,
                    resolved: true,
                    ..
                } => {
                    self.resolve_signal_comment(route, signal_id, &events[..=index])
                        .await;
                }
                TaskHistoryEvent::OperatorSignalConfirmed { signal_id } => {
                    self.resolve_signal_comment(route, signal_id, &events[..=index])
                        .await;
                    return self.finish_accepted(route, &events[..=index]).await;
                }
                TaskHistoryEvent::TaskCompleted { .. } => {
                    self.resolve_signal_comments(route, &events[..=index]).await;
                    return self.finish_accepted(route, &events[..=index]).await;
                }
                _ => {}
            }
            self.set_cursor(&route.issue_id, index + 1)?;
        }
        if folded {
            // The destination still owes the operator whatever is waiting, even
            // though the rest of the history was already reported at the source.
            let contributions = contribution_details(&events);
            for signal in host.unresolved_signals(route.task_id.clone()).await? {
                if !unresolved.contains(&signal.id) {
                    continue;
                }
                let Some(message) = signal.latest_agent_message() else {
                    continue;
                };
                self.publish_signal(
                    route,
                    &signal.id,
                    signal.kind,
                    &signal.title,
                    &message.body,
                    &contributions,
                )
                .await?;
            }
            self.set_cursor(&route.issue_id, events.len())?;
        }
        Ok(())
    }

    /// The task moved to another machine: say so and drop the route without
    /// stopping the work or marking the issue Done.
    async fn finish_transferred(&self, route: &Route) -> Result<(), String> {
        self.config
            .automation
            .create_activity(
                &route.session_id,
                &stable_id(&route.task_id, "transferred"),
                AgentActivityContent::Response {
                    body: "This task moved to another remote and keeps running there.".to_owned(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        self.retire_route(route)
    }

    async fn publish_signal(
        &self,
        route: &Route,
        signal_id: &str,
        kind: TaskOperatorSignalKind,
        title: &str,
        body: &str,
        contributions: &[String],
    ) -> Result<(), String> {
        let reference = self.issue_reference(&route.issue_id);
        let comment_id = stable_id(
            "comment",
            &format!("signal:{}:{}", route.task_id, signal_id),
        );
        match kind {
            // Updates are the session's transcript, not an issue comment.
            TaskOperatorSignalKind::Update => {
                let body = format!("**○ Info:** {}\n\n{}", title.trim(), body.trim());
                self.config
                    .automation
                    .create_activity(
                        &route.session_id,
                        &stable_id(&route.task_id, signal_id),
                        AgentActivityContent::Thought { body },
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
            TaskOperatorSignalKind::Decision => {
                let body = format!("**→ Decision:** {}\n\n{}", title.trim(), body.trim());
                self.config
                    .automation
                    .comment_on_issue(&route.issue_id, &body, &comment_id)
                    .await
                    .map_err(|error| error.to_string())
            }
            TaskOperatorSignalKind::Blocked => {
                let body = format!("**? Needs you:** {}\n\n{}", title.trim(), body.trim());
                self.config
                    .automation
                    .comment_on_issue(&route.issue_id, &body, &comment_id)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Err(error) = self
                    .config
                    .automation
                    .status(&reference, TrackerStatus::InReview)
                    .await
                {
                    tracing::warn!(issue = %route.issue_id, %error, "signal comment posted without an attention status");
                }
                Ok(())
            }
            TaskOperatorSignalKind::InReview => {
                let mut body =
                    format!("**◆ Ready to review:** {}\n\n{}", title.trim(), body.trim());
                append_contributions(&mut body, contributions);
                self.config
                    .automation
                    .comment_on_issue(&route.issue_id, &body, &comment_id)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Err(error) = self
                    .config
                    .automation
                    .status(&reference, TrackerStatus::InReview)
                    .await
                {
                    tracing::warn!(issue = %route.issue_id, %error, "review comment posted without an In Review status");
                }
                Ok(())
            }
        }
    }

    /// Resolve the comment a signal was published as, if it has one.
    async fn resolve_signal_comment(
        &self,
        route: &Route,
        signal_id: &str,
        events: &[(i64, TaskHistoryEvent)],
    ) {
        let Some(kind) = signal_kind(events, signal_id) else {
            return;
        };
        if kind == TaskOperatorSignalKind::Update {
            return;
        }
        let comment_id = stable_id(
            "comment",
            &format!("signal:{}:{}", route.task_id, signal_id),
        );
        if let Err(error) = self
            .config
            .automation
            .resolve_comment(&comment_id, None)
            .await
        {
            tracing::warn!(task = %route.task_id, signal = signal_id, %error, "could not resolve a signal comment");
        }
    }

    async fn resolve_signal_comments(&self, route: &Route, events: &[(i64, TaskHistoryEvent)]) {
        let mut seen = HashSet::new();
        for (_, event) in events {
            let TaskHistoryEvent::OperatorSignalRaised { signal } = event else {
                continue;
            };
            if signal.kind == TaskOperatorSignalKind::Update || !seen.insert(signal.id.clone()) {
                continue;
            }
            self.resolve_signal_comment(route, &signal.id, events).await;
        }
    }

    /// The operator accepted: report Done, say so once, and drop the route.
    async fn finish_accepted(
        &self,
        route: &Route,
        events: &[(i64, TaskHistoryEvent)],
    ) -> Result<(), String> {
        self.config
            .automation
            .status(&self.issue_reference(&route.issue_id), TrackerStatus::Done)
            .await
            .map_err(|error| error.to_string())?;
        self.config
            .automation
            .create_activity(
                &route.session_id,
                &stable_id(&route.task_id, "accepted"),
                AgentActivityContent::Response {
                    body: accepted_body(&contribution_urls(events)),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        self.retire_route(route)
    }

    /// An unknown delegation is educated once, not run.
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
             3. In Settings → Identities, sign in to {tracker}, so Altitude knows whose work \
             this is.\n\n\
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

    /// The tracker user a peer is signed in as, from what that peer pushed.
    fn peer_for_user(&self, tracker_user_id: &str) -> Option<String> {
        let mut matches = self
            .accounts()
            .into_iter()
            .filter(|(_, record)| record.account.user_id == tracker_user_id);
        let (first_peer, first) = matches.next()?;
        // The newest account wins, and a tie is broken by the peer's own key so
        // the answer does not depend on directory order.
        matches
            .fold((first_peer, first), |best, candidate| {
                if candidate.1.changed_at_ms > best.1.changed_at_ms
                    || (candidate.1.changed_at_ms == best.1.changed_at_ms && candidate.0 < best.0)
                {
                    candidate
                } else {
                    best
                }
            })
            .0
            .into()
    }

    /// Whether this peer has synced a tracker account, which is what a route
    /// needs to be answerable.
    pub fn has_account(&self, peer: EndpointId) -> bool {
        self.user_for_peer(&peer.to_string()).is_some()
    }

    /// The tracker user a peer is signed in as, from what it pushed.
    fn user_for_peer(&self, peer: &str) -> Option<String> {
        self.accounts()
            .into_iter()
            .find(|(owner, _)| owner == peer)
            .map(|(_, record)| record.account.user_id)
    }

    /// Every peer's synced tracker account, read from what each pushed.
    ///
    /// Read from disk rather than cached: the account is the peer's, and a peer
    /// that has not synced yet is simply absent.
    fn accounts(&self) -> Vec<(String, AccountRecord)> {
        let Ok(entries) = fs::read_dir(&self.config.operators_dir) else {
            return Vec::new();
        };
        let mut accounts = Vec::new();
        for entry in entries.flatten() {
            let peer = entry.file_name().to_string_lossy().into_owned();
            let Ok(text) = fs::read_to_string(accounts_path(&entry.path())) else {
                continue;
            };
            let Ok(records) = serde_json::from_str::<Vec<AccountRecord>>(&text) else {
                continue;
            };
            for record in records {
                if record.account.provider == TrackerKind::Linear {
                    accounts.push((peer.clone(), record));
                }
            }
        }
        accounts
    }

    fn route_for_task(&self, task_id: &str) -> Option<Route> {
        let ledger = self.lock().ok()?;
        ledger
            .routes
            .iter()
            .find(|route| route.task_id == task_id)
            .cloned()
    }

    fn route_for_session(&self, session_id: &str) -> Option<Route> {
        let ledger = self.lock().ok()?;
        ledger
            .routes
            .iter()
            .find(|route| route.session_id == session_id)
            .cloned()
    }

    fn route_for_issue(&self, issue_id: &str) -> Option<Route> {
        let ledger = self.lock().ok()?;
        ledger
            .routes
            .iter()
            .find(|route| route.issue_id == issue_id)
            .cloned()
    }

    fn routes(&self) -> Vec<Route> {
        self.lock()
            .map(|ledger| ledger.routes.clone())
            .unwrap_or_default()
    }

    fn upsert_route(&self, route: Route) -> Result<(), String> {
        self.mutate(|ledger| {
            ledger
                .routes
                .retain(|existing| existing.issue_id != route.issue_id);
            ledger.routes.push(route);
        })
    }

    /// Remove a route and remember the session so a replay cannot restart it.
    fn retire_route(&self, route: &Route) -> Result<(), String> {
        let now = now_ms();
        self.mutate(|ledger| {
            ledger
                .routes
                .retain(|existing| existing.issue_id != route.issue_id);
            ledger
                .terminated_sessions
                .retain(|session| session.session_id != route.session_id);
            ledger.terminated_sessions.push(TerminatedSession {
                session_id: route.session_id.clone(),
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

    fn set_cursor(&self, issue_id: &str, cursor: usize) -> Result<(), String> {
        self.mutate(|ledger| {
            if let Some(route) = ledger.routes.iter_mut().find(|r| r.issue_id == issue_id) {
                route.cursor = cursor;
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

/// An `Issue` webhook, as far as the coordinator trusts it.
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

/// The tracker account a peer pushed, and when it last changed.
///
/// The stamp follows the account, not the push: a login refreshes underneath the
/// profile it travels in, and a refreshed login must not re-assert a binding the
/// operator has since moved to another machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub account: TrackerAccount,
    pub changed_at_ms: i64,
}

/// Where a peer's tracker accounts are kept, beside the configuration it pushed.
pub fn accounts_path(operator_dir: &Path) -> PathBuf {
    operator_dir.join("accounts.json")
}

/// Stop a peer's record naming a provider, because another client took it over.
///
/// What that peer pushed stays where it is: the login is the same account, and
/// which client a task is attributed to is the question here, not who holds the
/// credentials.
pub fn forget_account(operator_dir: &Path, provider: TrackerKind) -> io::Result<()> {
    let path = accounts_path(operator_dir);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let held: Vec<AccountRecord> = serde_json::from_str(&text).unwrap_or_default();
    let kept: Vec<&AccountRecord> = held
        .iter()
        .filter(|record| record.account.provider != provider)
        .collect();
    if kept.len() == held.len() {
        return Ok(());
    }
    let text = serde_json::to_string_pretty(&kept).map_err(io::Error::other)?;
    super::identity::write_private(&path, &text).map_err(io::Error::other)
}

/// Note the tracker accounts a peer's profiles now name.
///
/// `pushed` carries one entry per tracker profile in the configuration that just
/// arrived: the account it names, or nothing when the peer has signed out. A
/// provider that was not in this push keeps the account it already had.
pub fn record_accounts(
    operator_dir: &Path,
    pushed: &[(TrackerKind, Option<TrackerAccount>)],
) -> io::Result<()> {
    let path = accounts_path(operator_dir);
    let held: Vec<AccountRecord> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let now = now_ms();
    let mut records: Vec<AccountRecord> = pushed
        .iter()
        .filter_map(|(_, account)| account.as_ref())
        .map(|account| AccountRecord {
            changed_at_ms: held
                .iter()
                .find(|record| record.account == *account)
                .map_or(now, |record| record.changed_at_ms),
            account: account.clone(),
        })
        .collect();
    records.extend(
        held.iter()
            .filter(|record| {
                !pushed
                    .iter()
                    .any(|(provider, _)| *provider == record.account.provider)
            })
            .cloned(),
    );
    // A pushed login refreshing underneath an unchanged account must not look
    // like the account changed, so nothing is written when nothing moved.
    if records == held {
        return Ok(());
    }
    if records.is_empty() {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    let text = serde_json::to_string_pretty(&records)
        .map_err(|error| io::Error::other(format!("could not encode tracker accounts: {error}")))?;
    write_private(&path, &text).map_err(io::Error::other)
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    deliveries: Vec<Delivery>,
    #[serde(default)]
    routes: Vec<Route>,
    #[serde(default)]
    terminated_sessions: Vec<TerminatedSession>,
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
struct Route {
    issue_id: String,
    session_id: String,
    peer: String,
    tracker_user_id: String,
    task_id: String,
    #[serde(default)]
    cursor: usize,
    #[serde(default)]
    origin: RouteOrigin,
}

/// How a route came to exist, which decides whether it depends on the issue
/// still being delegated to this app.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RouteOrigin {
    /// Linear delegated the issue to the app, so removing that delegation ends
    /// the route.
    #[default]
    Delegated,
    /// A client started the task and this remote only reports it, so the
    /// issue need not stay delegated and a native stop is still accepted.
    Piloted,
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

    /// The signed content that makes a redelivery the same work.
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
