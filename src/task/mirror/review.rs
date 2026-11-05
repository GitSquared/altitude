use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use tokio::{sync::Mutex, time};

use serde_json::{Value, json};

use crate::{
    isolation::{IsolationEngine, WATCHER_PREFIX},
    vcs::{CodeContribution, ContributionObservation, ProviderBatch},
};

use crate::task::{TaskError, TaskOperatorSignal, TaskService, TaskServiceError};

pub(crate) const REVIEW_POLL_INTERVAL: Duration = Duration::from_secs(30);

const REVIEW_POLL_CEILING: Duration = Duration::from_secs(600);

const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) const OBSERVATION_ATTEMPTS: u32 = 5;

pub(crate) enum Look {
    Read(Vec<ContributionObservation>),
    Unreadable,
    NotAsked,
    GivenUp,
}

pub(crate) struct Watchers<E> {
    service: TaskService<E>,
    owners: Mutex<HashMap<String, Owner>>,
    stopped: AtomicBool,
}

#[derive(Default)]
struct Owner {
    sandbox: Option<String>,
    failures: u32,
    retry_at: Option<Instant>,
    last_failure: Option<String>,
}

impl<E> Watchers<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub(crate) fn new(service: TaskService<E>) -> Self {
        Self {
            service,
            owners: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(crate) async fn look(&self, owner: &str, contributions: &[CodeContribution]) -> Look {
        let mut owners = self.owners.lock().await;
        if self.is_stopped() {
            return Look::NotAsked;
        }
        let entry = owners.entry(owner.to_owned()).or_default();
        if entry.failures >= OBSERVATION_ATTEMPTS {
            return Look::GivenUp;
        }
        if entry.retry_at.is_some_and(|at| at > Instant::now()) {
            return Look::NotAsked;
        }
        // A sandbox that failed to be made is made again rather than reused, and
        // so is one a read failed through.
        let sandbox = match entry.sandbox.clone() {
            Some(id) => Some(id),
            None => match self.open(owner).await {
                Ok(id) => {
                    entry.sandbox = Some(id.clone());
                    Some(id)
                }
                Err(error) => {
                    tracing::warn!(%owner, %error, "could not prepare a watcher sandbox");
                    None
                }
            },
        };
        let read = match &sandbox {
            Some(id) => self.read(id, contributions).await,
            None => Err("no sandbox could be made to ask in".to_owned()),
        };
        match read {
            Ok(observations) => {
                entry.failures = 0;
                entry.retry_at = None;
                Look::Read(observations)
            }
            Err(reason) => {
                self.failed(owner, entry, reason).await;
                Look::Unreadable
            }
        }
    }

    pub(crate) async fn last_failure(&self, owner: &str) -> Option<String> {
        self.owners
            .lock()
            .await
            .get(owner)
            .and_then(|entry| entry.last_failure.clone())
    }

    pub(crate) async fn retain(&self, owners: &HashSet<String>) {
        let mut live = self.owners.lock().await;
        let dropped: Vec<String> = live
            .keys()
            .filter(|owner| !owners.contains(*owner))
            .cloned()
            .collect();
        for owner in dropped {
            if let Some(id) = live.remove(&owner).and_then(|owner| owner.sandbox) {
                self.remove(&id).await;
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        let ids: Vec<String> = self
            .owners
            .lock()
            .await
            .drain()
            .filter_map(|(_, owner)| owner.sandbox)
            .collect();
        for id in ids {
            self.remove(&id).await;
        }
    }

    async fn failed(&self, owner: &str, entry: &mut Owner, reason: String) {
        entry.failures += 1;
        entry.retry_at = Some(Instant::now() + backoff(entry.failures));
        entry.last_failure = Some(reason.clone());
        tracing::warn!(
            %owner,
            failures = entry.failures,
            %reason,
            "could not read linked contributions"
        );
        if let Some(id) = entry.sandbox.take() {
            self.remove(&id).await;
        }
    }

    async fn open(&self, owner: &str) -> Result<String, TaskServiceError> {
        let (operator, credential) = owner
            .split_once('\0')
            .ok_or_else(|| TaskServiceError::Task(TaskError::MissingProject))?;
        let service = self.service.as_operator(operator)?;
        let id = watcher_sandbox_id(owner);
        service.open_watcher(id.clone(), credential).await?;
        Ok(id)
    }

    async fn read(
        &self,
        sandbox_id: &str,
        contributions: &[CodeContribution],
    ) -> Result<Vec<ContributionObservation>, String> {
        let mut observations = Vec::with_capacity(contributions.len());
        for batch in ProviderBatch::of(contributions) {
            let command = batch.command();
            let read =
                match time::timeout(READ_TIMEOUT, self.service.run_captured(sandbox_id, command))
                    .await
                {
                    Ok(Ok(read)) => read,
                    Ok(Err(error)) => return Err(error.to_string()),
                    Err(_) => {
                        return Err(format!(
                            "the provider did not answer within {}s",
                            READ_TIMEOUT.as_secs()
                        ));
                    }
                };
            // What came back is the provider's words, so a reply this machine
            // cannot read is quoted rather than summarised as "unreadable".
            let parsed = batch.observations(&read).ok_or_else(|| {
                format!(
                    "the provider's answer was not what this machine expected: {}",
                    tail(&read, 200)
                )
            })?;
            observations.extend(parsed);
        }
        Ok(observations)
    }

    async fn remove(&self, id: &str) {
        if let Err(error) = self.service.isolation.remove(id.to_owned()).await {
            tracing::debug!(sandbox_id = %id, %error, "no watcher sandbox to remove");
        }
    }
}

fn backoff(failures: u32) -> Duration {
    let steps = failures.saturating_sub(1).min(OBSERVATION_ATTEMPTS - 1);
    REVIEW_POLL_INTERVAL * 2u32.pow(steps)
}

#[derive(Default)]
pub(crate) struct ReviewWaits {
    waits: Mutex<HashMap<String, ReviewWait>>,
}

impl ReviewWaits {
    pub(crate) async fn due(&self, watched: &HashSet<String>) -> HashSet<String> {
        let now = Instant::now();
        let mut waits = self.waits.lock().await;
        waits.retain(|task_id, _| watched.contains(task_id));
        watched
            .iter()
            .filter(|task_id| {
                waits
                    .entry((*task_id).clone())
                    .or_insert_with(ReviewWait::first_look)
                    .is_due(now)
            })
            .cloned()
            .collect()
    }

    pub(crate) async fn looked(&self, task_id: &str, moved: bool) {
        if let Some(wait) = self.waits.lock().await.get_mut(task_id) {
            if moved {
                wait.moved();
            } else {
                wait.unchanged();
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ReviewWait {
    next_at: Instant,
    interval: Duration,
}

impl ReviewWait {
    fn first_look() -> Self {
        Self {
            next_at: Instant::now(),
            interval: REVIEW_POLL_INTERVAL,
        }
    }

    fn is_due(&self, now: Instant) -> bool {
        now >= self.next_at
    }

    fn moved(&mut self) {
        self.interval = REVIEW_POLL_INTERVAL;
        self.next_at = Instant::now() + self.interval;
    }

    fn unchanged(&mut self) {
        self.interval = (self.interval * 2).min(REVIEW_POLL_CEILING);
        self.next_at = Instant::now() + self.interval;
    }
}

// What the agent is told when its pull requests are not ready: the state of each
// one that needs something, as data. Prose would have to pick what matters and the
// agent would have to parse it back; this says everything the provider said, in the
// shape the provider said it, and the agent reads what it needs. It is also exactly
// what the machine compares to know whether it is repeating itself, so two equal
// messages mean two equal states.
pub(crate) fn news(needing_action: &[ContributionObservation]) -> String {
    let pull_requests: Vec<Value> = needing_action
        .iter()
        .map(|observation| {
            let state = &observation.state;
            json!({
                "pull_request": observation.contribution_id,
                "head": state.head_sha.chars().take(7).collect::<String>(),
                "merge_state": state.standing.label(),
                "checks": {
                    "failing": state.failing_checks,
                    "pending": state.pending_checks,
                    "passing": state.checks_passing,
                },
                "last_review": state.latest_review.as_ref().map(|review| json!({
                    "state": review.verdict,
                    "author": review.author,
                    "bot": review.is_bot,
                })),
                "comments": {
                    "total": state.comment_count,
                    "newest": state.latest_comment.as_ref().map(|comment| json!({
                        "id": comment.id,
                        "author": comment.author,
                        "bot": comment.is_bot,
                        "at": comment.at,
                    })),
                },
            })
        })
        .collect();
    // Pretty-printed because it is read by a model and by whoever opens the task's
    // history afterwards, and neither is helped by one long line.
    serde_json::to_string_pretty(&json!({ "pull_requests": pull_requests }))
        .unwrap_or_else(|_| "{}".to_owned())
}

pub(crate) fn unreadable(signal: &TaskOperatorSignal, reason: &str) -> TaskOperatorSignal {
    let mut signal = signal.clone();
    signal.body = format!(
        "{}\n\nAltitude could not read this task's pull requests after {OBSERVATION_ATTEMPTS} attempts, so it has stopped waiting on them. What the provider said: {reason}",
        signal.body
    );
    signal
}

fn tail(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_owned();
    }
    let skip = trimmed.chars().count() - limit;
    format!("…{}", trimmed.chars().skip(skip).collect::<String>())
}

fn watcher_sandbox_id(owner: &str) -> String {
    format!(
        "{WATCHER_PREFIX}{}",
        &blake3::hash(owner.as_bytes()).to_hex()[..16]
    )
}
