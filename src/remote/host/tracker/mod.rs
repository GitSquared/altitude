use crate::remote::StartFuture;
pub use logins::{Login, claim_account, logins, logins_of, yield_account};
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::admission::{self};
use crate::remote::identity::write_private;
use crate::{
    task::{
        TaskExecutionPauseReason, TaskHistoryEvent, TaskOperatorSignalKind,
        TaskOperatorSignalThread,
    },
    tracker::{
        IssueReference, IssueSnapshot, TrackerStatus,
        providers::linear::automation::LinearAutomation,
    },
};
use axum::{Router, extract::DefaultBodyLimit, routing::post};
use iroh::EndpointId;
use ledger::{IssueStateChanged, Ledger, StoredEvent};
use tokio::{
    sync::Notify,
    time::{MissedTickBehavior, interval},
};

mod answering;
mod delegation;
mod delivery;
mod intake;
mod ledger;
mod logins;
mod records;
mod reporting;
mod webhook;

// How old a delegation may be and still be picked up by a sweep. Two days is long
// enough to cover a machine that was off for a weekend and short enough that an
// issue delegated weeks ago stays a record rather than becoming work again.
const STALE_DELEGATION_MS: i64 = 48 * 60 * 60 * 1000;

const WEBHOOK_ROUTE: &str = "/altitude/linear/webhook";

const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;

const SIGNATURE_HEADER: &str = "linear-signature";

const TICK: Duration = Duration::from_secs(10);

const QUEUE_NOTICE_INTERVAL: Duration = Duration::from_secs(300);

const NEXT_UP_NOTICE_INTERVAL: Duration = Duration::from_secs(30);

const DELEGATION_SWEEP_TICKS: u32 = 6;

const EVENT_ATTEMPTS: usize = 3;

const UNHOOK_GRACE_MS: i64 = 60_000;

const RETRY: Duration = Duration::from_secs(10);

const DONE_RETENTION: Duration = Duration::from_secs(600);

const TERMINATED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

const MAX_DONE: usize = 512;

const MAX_TERMINATED: usize = 1024;

const WEBHOOK_MAX_AGE: Duration = Duration::from_secs(300);

pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub struct RemoteTrackerConfig {
    pub automation: Arc<LinearAutomation>,
    pub webhook_secret: Vec<u8>,
    pub oauth_client_id: String,
    pub storage_path: PathBuf,
    pub attachments_dir: PathBuf,
    pub operators_dir: PathBuf,
    pub host_label: String,
}

pub struct BoundTask {
    pub task_id: String,
    pub owner: String,
    pub issue: IssueReference,
    pub delegation: Option<String>,
    pub session_agent: Option<String>,
}

use intake::webhook as intake_webhook;
use reporting::{ActivityNotice, QueueNotice};

pub struct RemoteTracker {
    config: RemoteTrackerConfig,
    queue_notices: Mutex<HashMap<String, QueueNotice>>,
    activity_notices: Mutex<HashMap<String, ActivityNotice>>,
    ledger: Mutex<Ledger>,
    notify: Notify,
    refusals: Mutex<HashMap<String, String>>,
    delivered: AtomicBool,
}

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
    fn steer(&self, task_id: String, message: String) -> HostFuture<'_, ()>;
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
    fn current_activity(&self, task_id: String) -> HostFuture<'_, Option<String>>;
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
        // The one pause nobody has to clear. It names the clock's owner and not the
        // hour, because a time is right for one machine's timezone and not another's.
        TaskExecutionPauseReason::AwaitingLimit => {
            "The account's usage limit is used up. This task resumes on its own when the \
             limit clears; nothing here is waiting on you."
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

fn picking_up(operator: Option<&str>, agent: Option<&str>) -> String {
    let operator = operator.map(str::trim).filter(|name| !name.is_empty());
    // Altitude names its agents in lower case, which belongs mid-sentence in the
    // session summary and not in a sentence the operator reads.
    let agent = agent
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
        .map(|agent| {
            let mut characters = agent.chars();
            match characters.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), characters.as_str()),
                None => String::new(),
            }
        })
        .filter(|agent| !agent.is_empty());
    match (operator, agent) {
        (Some(operator), Some(agent)) => format!("{operator}'s {agent} is picking up this task."),
        (None, Some(agent)) => format!("{agent} is picking up this task."),
        (Some(operator), None) => format!("{operator}'s agent is picking up this task."),
        (None, None) => "Altitude is picking up this task.".to_owned(),
    }
}

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

impl RemoteTracker {
    pub fn new(config: RemoteTrackerConfig) -> Result<Arc<Self>, String> {
        let ledger = Ledger::load(&config.storage_path)?;
        Ok(Arc::new(Self {
            config,
            queue_notices: Mutex::new(HashMap::new()),
            activity_notices: Mutex::new(HashMap::new()),
            ledger: Mutex::new(ledger),
            refusals: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            delivered: AtomicBool::new(false),
        }))
    }

    pub fn webhook_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(WEBHOOK_ROUTE, post(intake_webhook))
            .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BYTES))
            .with_state(Arc::clone(self))
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
            self.report_activity(host.as_ref()).await;
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
}
