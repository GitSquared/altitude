pub use connect::{connect, credentials_path, readiness};
pub use setup::connect_remote;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::remote::identity::write_private;
use crate::task::{TaskAskKind, TaskExecutionPauseReason, TaskHistoryEvent};
use crate::tracker::host::{BoundTask, DelegatedWork, QueueStanding, Start};
use crate::tracker::providers::linear::automation::{LinearAutomation, is_absence};
use axum::{Router, extract::DefaultBodyLimit, routing::post};
use ledger::{IssueStateChanged, Ledger, StoredEvent};
use std::time::Instant;
use tokio::{sync::Notify, time};

mod answering;
mod connect;
mod delegation;
mod delivery;
mod intake;
mod ledger;
mod records;
mod reporting;
mod setup;
mod webhook;

// How old a delegation may be and still be picked up by a sweep. Two days is long
// enough to cover a machine that was off for a weekend and short enough that an
// issue delegated weeks ago stays a record rather than becoming work again.
const STALE_DELEGATION_MS: i64 = 48 * 60 * 60 * 1000;

const WEBHOOK_ROUTE: &str = "/altitude/linear/webhook";

const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;

const SIGNATURE_HEADER: &str = "linear-signature";

// A burst of task activity arrives faster than a pass is worth running: the passes
// read whole task histories and talk to the tracker, so what a burst needs is one pass
// after it, not one per line. At rest nothing waits on this, because nothing wakes the
// loop then.
const SETTLE: Duration = Duration::from_secs(1);

const QUEUE_NOTICE_INTERVAL: Duration = Duration::from_secs(300);

const NEXT_UP_NOTICE_INTERVAL: Duration = Duration::from_secs(30);

const EVENT_ATTEMPTS: usize = 3;

const UNHOOK_GRACE_MS: i64 = 60_000;

// How old a task's history has to be, relative to the report that watches it, for the
// report to be adopting a task rather than starting one: a delegation's task is
// created in the same breath as its report.
const ADOPTION_WINDOW_MS: i64 = 60_000;

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
    pub attachments_dir: PathBuf,
    pub operators_dir: PathBuf,
    pub host_label: String,
}

use intake::webhook as intake_webhook;
use reporting::QueueNotice;

pub struct RemoteTracker {
    config: RemoteTrackerConfig,
    queue_notices: Mutex<HashMap<String, QueueNotice>>,
    ledger: Mutex<Ledger>,
    notify: Notify,
    // When this process started. It names the activities it publishes, so a restart
    // cannot reuse an id Linear already has and be folded into something nobody saw.
    started_at_ms: i64,
    refusals: Mutex<HashMap<String, String>>,
    delivered: AtomicBool,
}

// An error activity ends the session's "working" state, so it says what stopped the
// task rather than only that something did. A failure is a state and not a question,
// which is why the way out of it is part of the same message.
fn failure_notice(error: &str) -> String {
    format!(
        "**altitude stopped this task.**\n\n{}\n\n\
         Take it over with ctrl-enter in the altitude terminal and unblock it there: leaving \
         the terminal hands the work back to altitude, which carries on from where you left \
         it. A message here restarts it too, without a terminal.",
        error.trim()
    )
}

// A pause that asks a person for something is the last thing a run says, and it is
// addressed to them. The rest are the machine's own record: they explain a silence
// without asking anything, so they are written where they do not read as a message.
//
// A run that kept taking turns with nothing to say stopped itself, so it asks nobody:
// putting that in the operator's inbox says someone needs them when nobody does.
fn waits_on_a_person(reason: &TaskExecutionPauseReason) -> bool {
    matches!(reason, TaskExecutionPauseReason::AwaitingOperator)
}

// The handoff, in the operator's own name: a tracker is shared with a team, and "you"
// is only one of the people reading it.
fn pause_message(
    reason: &TaskExecutionPauseReason,
    operator: &str,
    asking: Option<TaskAskKind>,
) -> String {
    match (reason, asking) {
        (TaskExecutionPauseReason::AwaitingOperator, Some(TaskAskKind::Steer)) => {
            format!("Waiting for {operator} on the question.")
        }
        (TaskExecutionPauseReason::AwaitingOperator, Some(TaskAskKind::Review)) => {
            format!("Waiting for {operator} on the review.")
        }
        (TaskExecutionPauseReason::AwaitingOperator, None) => format!("Waiting for {operator}."),
        (reason, _) => format!("{} Tell {operator} what to do next.", reason.notice()),
    }
}

fn picking_up(operator: Option<&str>, agent: Option<&str>) -> String {
    let operator = operator.map(str::trim).filter(|name| !name.is_empty());
    // altitude names its agents in lower case, which belongs mid-sentence in the
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
        (None, None) => "altitude is picking up this task.".to_owned(),
    }
}

fn persist(path: &Path, ledger: &Ledger) -> Result<(), String> {
    let text = serde_json::to_string_pretty(ledger)
        .map_err(|error| format!("could not encode the tracker ledger: {error}"))?;
    write_private(path, &text)
        .map_err(|error| format!("could not write the tracker ledger: {error}"))
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
            ledger: Mutex::new(ledger),
            refusals: Mutex::new(HashMap::new()),
            notify: Notify::new(),
            started_at_ms: now_ms(),
            delivered: AtomicBool::new(false),
        }))
    }

    pub fn webhook_router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(WEBHOOK_ROUTE, post(intake_webhook))
            .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BYTES))
            .with_state(Arc::clone(self))
    }

    // Anything that changed what the tracker owes: a delivery arrived, or a task's
    // history moved.
    pub fn wake(&self) {
        self.notify.notify_one();
    }

    // Nothing here runs on a clock. A webhook, a task's history moving, or a moment
    // something already knows it has to happen at — a delivery that is being retried,
    // a queue notice that is worth repeating — are the only reasons to do anything,
    // and until one of them arrives this loop waits.
    pub async fn run(self: Arc<Self>, host: Arc<dyn DelegatedWork>) {
        // The two things that are not events, both of them once: a delegation made
        // while this machine was not listening left no webhook to find, and a session a
        // previous run was talking into may have been finished with since.
        self.recover_delegations(host.as_ref()).await;
        self.verify_sessions(host.as_ref()).await;
        loop {
            let due = self.pass(host.as_ref()).await;
            let done_at = Instant::now();
            match due {
                Some(at) => {
                    tokio::select! {
                        _ = self.notify.notified() => {}
                        _ = time::sleep_until(at.into()) => {}
                    }
                }
                None => self.notify.notified().await,
            }
            // A burst of lines from one run is one pass after it rather than one pass
            // per line. Only a wake reaches this, so the wait costs nothing at rest.
            if let Some(rest) = SETTLE.checked_sub(done_at.elapsed()) {
                time::sleep(rest).await;
            }
        }
    }

    // Everything that is due, in the order it has to happen, and when it should be
    // asked again if that is a moment rather than an event.
    async fn pass(&self, host: &dyn DelegatedWork) -> Option<Instant> {
        self.repair_sessions(host).await;
        let retry = self.process_deliveries(host).await;
        self.adopt_bound_tasks(host).await;
        let notice = self.report_queue_standing(host).await;
        self.report_activity(host).await;
        self.poll_reports(host).await;
        [retry, notice].into_iter().flatten().min()
    }
}
