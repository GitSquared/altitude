//! Watching the linked contributions of wrap-up requests the host is holding.
//!
//! One scheduler owns every held review, and one sandbox per credential owner
//! answers for all of them. A held task has no capsule — its work is in a
//! snapshot and its place in the queue is the scheduler's to decide — so this is
//! where a review request waits without an agent running to wait with it.

use std::{
    collections::{HashMap, HashSet},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use tokio::{sync::Mutex, time};

use crate::{
    isolation::{IsolationEngine, WATCHER_PREFIX},
    vcs::{CodeContribution, ContributionObservation, ProviderBatch},
};

use super::{TaskOperatorSignal, TaskService, TaskServiceError};

/// How long a held review waits before its first look, and the shortest wait
/// after that.
pub(super) const REVIEW_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// The longest a review whose contributions have not moved will wait.
///
/// A contribution that says the same thing every time is asked about less and
/// less often, so a pull request whose checks take an hour does not cost the
/// provider a request every thirty seconds. Waiting longer costs only how late a
/// change is noticed.
const REVIEW_POLL_CEILING: Duration = Duration::from_secs(600);

/// How long one read may take before it counts as a provider that cannot be
/// read.
///
/// An owner's watching is one thing at a time, so a command that hung on a
/// stalled connection would hold that owner — and the host's shutdown — for as
/// long as the connection stayed open.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// How many times a provider that cannot be read is tried before the host stops
/// waiting on it.
pub(super) const OBSERVATION_ATTEMPTS: u32 = 5;

pub(super) enum Look {
    Read(Vec<ContributionObservation>),
    Unreadable,
    /// The provider was not asked, because the host is stopping or because its
    /// recent reads failed and it is waiting out a backoff.
    NotAsked,
    /// The provider could not be read often enough to keep waiting on. The
    /// reviews it holds belong to the operator now: only they can sign it in
    /// again.
    GivenUp,
}

/// What the host reads contributions with, one entry per credential owner.
///
/// The sandbox is kept while the owner has reviews waiting and dropped when it
/// does not: the environment snapshot stays on disk, so a sandbox is restored
/// from it in seconds and holding one nobody is waiting on buys nothing. Each is
/// scoped to its owner before it is made, so a machine serving several peers
/// never lets one peer's `gh` login answer for another's.
pub(super) struct Watchers<E> {
    service: TaskService<E>,
    owners: Mutex<HashMap<String, Owner>>,
    stopped: AtomicBool,
}

#[derive(Default)]
struct Owner {
    sandbox: Option<String>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl<E> Watchers<E>
where
    E: IsolationEngine + Clone,
{
    pub(super) fn new(service: TaskService<E>) -> Self {
        Self {
            service,
            owners: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        }
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Read every one of an owner's linked contributions, or say why not.
    ///
    /// One contribution that cannot be read is the whole look failing: a partial
    /// picture must not decide that a pull request is ready. An owner's watching
    /// is one thing at a time, so the lock is held across the read.
    pub(super) async fn look(&self, owner: &str, contributions: &[CodeContribution]) -> Look {
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
            None => None,
        };
        match read {
            Some(observations) => {
                entry.failures = 0;
                entry.retry_at = None;
                Look::Read(observations)
            }
            None => {
                self.failed(owner, entry).await;
                Look::Unreadable
            }
        }
    }

    pub(super) async fn retain(&self, owners: &HashSet<String>) {
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

    pub(super) async fn shutdown(&self) {
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

    /// Record that a look failed, and throw away the sandbox it failed through.
    ///
    /// A sandbox that cannot answer is not reused: the next attempt reads through
    /// a machine made again, which is what a wedged one needs. A login that is
    /// gone is not fixed by that, which is why the attempts are also spaced out,
    /// and why the host eventually gives up on them.
    async fn failed(&self, owner: &str, entry: &mut Owner) {
        entry.failures += 1;
        entry.retry_at = Some(Instant::now() + backoff(entry.failures));
        tracing::warn!(
            %owner,
            failures = entry.failures,
            "could not read linked contributions"
        );
        if let Some(id) = entry.sandbox.take() {
            self.remove(&id).await;
        }
    }

    async fn open(&self, owner: &str) -> Result<String, TaskServiceError> {
        let service = self.service.as_operator(owner)?;
        let id = watcher_sandbox_id(owner);
        service.open_watcher(id.clone()).await?;
        Ok(id)
    }

    async fn read(
        &self,
        sandbox_id: &str,
        contributions: &[CodeContribution],
    ) -> Option<Vec<ContributionObservation>> {
        let mut observations = Vec::with_capacity(contributions.len());
        for batch in ProviderBatch::of(contributions) {
            let command = batch.command();
            let read =
                match time::timeout(READ_TIMEOUT, self.service.run_captured(sandbox_id, command))
                    .await
                {
                    Ok(Ok(read)) => read,
                    Ok(Err(_)) | Err(_) => return None,
                };
            observations.extend(batch.observations(&read)?);
        }
        Some(observations)
    }

    async fn remove(&self, id: &str) {
        if let Err(error) = self.service.isolation.remove(id.to_owned()).await {
            tracing::debug!(sandbox_id = %id, %error, "no watcher sandbox to remove");
        }
    }
}

/// How long to wait after a failed look.
///
/// Doubling from the pass interval: a provider that is briefly unreachable is
/// tried again in half a minute, and one that is signed out is not asked about
/// every half minute for a quarter of an hour.
fn backoff(failures: u32) -> Duration {
    let steps = failures.saturating_sub(1).min(OBSERVATION_ATTEMPTS - 1);
    REVIEW_POLL_INTERVAL * 2u32.pow(steps)
}

/// When each held review is next worth a look.
///
/// Kept apart from [`Watchers`], which is per credential owner: a review waits on
/// its own contributions, and several reviews share one owner's sandbox.
#[derive(Default)]
pub(super) struct ReviewWaits {
    waits: Mutex<HashMap<String, ReviewWait>>,
}

impl ReviewWaits {
    /// The watched reviews worth looking at now.
    ///
    /// A review that is no longer watched is forgotten, so a resolved or failed
    /// one stops being counted.
    pub(super) async fn due(&self, watched: &HashSet<String>) -> HashSet<String> {
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

    pub(super) async fn looked(&self, task_id: &str, moved: bool) {
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

/// Why the host woke an agent from a held review, in the operator's words.
///
/// Only the contributions that need action are named: the ones that are ready
/// are not why the agent is being asked to work.
pub(super) fn wake_reason(needing_action: &[String]) -> String {
    format!(
        "A linked pull request needs attention before it can be reviewed: {}. Fix what is in scope and return the pull requests to a state a human can review.",
        needing_action.join("; ")
    )
}

/// A review request as it reads when the host gave up on reading the provider.
///
/// The request is the agent's words, so what the host could not do is appended
/// rather than replacing them.
pub(super) fn unreadable(signal: &TaskOperatorSignal) -> TaskOperatorSignal {
    let mut signal = signal.clone();
    signal.body = format!(
        "{}\n\nAltitude could not read this task's pull requests after {OBSERVATION_ATTEMPTS} attempts, so it has stopped waiting on them. The provider's command is most likely signed out on this machine.",
        signal.body
    );
    signal
}

/// The name of an owner's watcher sandbox.
///
/// Hashed rather than spelled out because an owner key is a long public key and
/// a sandbox name is a path component; the prefix is what says what it is, so a
/// leftover can be told from every other sandbox without knowing which owners
/// exist.
fn watcher_sandbox_id(owner: &str) -> String {
    format!(
        "{WATCHER_PREFIX}{}",
        &blake3::hash(owner.as_bytes()).to_hex()[..16]
    )
}
