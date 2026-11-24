use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::{Instant, sleep_until};

use super::freshness;
use super::{Credentials, Renewal};
use crate::{config::Config, isolation::IsolationEngine};

// A profile that failed is not asked again on the next tick: the first retry waits
// this long, and each failure doubles it up to the cap. An operator fixing a login
// is not a race, and a storm of sandboxes helps nobody.
const FIRST_RETRY: Duration = Duration::from_secs(60);
const LONGEST_RETRY: Duration = Duration::from_secs(60 * 30);
// A keeper wakes at the next deadline, but never sleeps longer than this, so a
// profile that appears on disk while it sleeps is noticed by the next pass.
const LONGEST_SLEEP: Duration = Duration::from_secs(60 * 15);

struct Backoff {
    failures: u32,
    retry_at: Instant,
    // Set when the harness said a sign-in is needed: asking again cannot change that,
    // only a new session can, so the profile waits for its session to differ from this
    // one rather than for a clock.
    refused_at: Option<Option<i64>>,
}

// A machine's renewal clock: it holds the profiles one scope owns and renews each
// before its own deadline, so a session is kept alive on an idle machine instead of
// only when a task happens to start. The deadlines are read from the profile files,
// which is what makes a restart recover: the first pass after it is the schedule.
pub struct Keeper<E> {
    credentials: Credentials<E>,
    backoff: HashMap<String, Backoff>,
}

impl<E> Keeper<E>
where
    E: IsolationEngine + Clone + 'static,
{
    pub fn new(credentials: Credentials<E>) -> Self {
        Self {
            credentials,
            backoff: HashMap::new(),
        }
    }

    // One pass over the profiles this scope holds: renew what is due, and say when
    // the next look is worth taking. The profiles are read again each pass, so one
    // that was added or forgotten is picked up without a restart.
    pub async fn poll(&mut self) -> Instant {
        let profiles = profiles(self.credentials.config());
        for profile in &profiles {
            self.poll_profile(profile).await;
        }
        self.next_wake(&profiles)
    }

    async fn poll_profile(&mut self, profile: &str) {
        // A refusal outlives the process: a restart that asked every refused credential
        // again would spend a sandbox on each, all at once, for the same answer.
        if !self.backoff.contains_key(profile)
            && let Some(seen) = self.recorded_refusal(profile)
        {
            if seen == self.credentials.session_expiry(profile) {
                self.backoff.insert(
                    profile.to_owned(),
                    Backoff {
                        failures: 0,
                        retry_at: Instant::now(),
                        refused_at: Some(seen),
                    },
                );
                return;
            }
            let _ = fs::remove_file(self.refusal_path(profile));
        }
        if let Some(backoff) = self.backoff.get(profile) {
            let waiting = match backoff.refused_at {
                Some(seen) => seen == self.credentials.session_expiry(profile),
                None => backoff.retry_at > Instant::now(),
            };
            if waiting {
                return;
            }
        }
        if !self.credentials.can_renew(profile) {
            self.backoff.remove(profile);
            return;
        }
        match self.credentials.renew_if_due(profile).await {
            Ok(Renewal::Renewed) => {
                self.backoff.remove(profile);
                tracing::info!(
                    profile,
                    expires_at_ms = self.credentials.session_expiry(profile),
                    "renewed a credential before it expired"
                );
            }
            Ok(Renewal::NotDue | Renewal::NothingToRenew) => {
                self.backoff.remove(profile);
            }
            Ok(Renewal::NoSession) => {
                self.park(profile);
                tracing::warn!(
                    profile,
                    "a credential has no session to renew; a sign-in is needed"
                );
            }
            Ok(Renewal::Refused(reason)) => {
                self.park(profile);
                tracing::warn!(
                    profile,
                    %reason,
                    "a credential could not be renewed; a sign-in is needed"
                );
            }
            Ok(Renewal::Unchanged) => {
                let retry = self.defer(profile);
                tracing::warn!(
                    profile,
                    retry_in_s = retry.as_secs(),
                    "a renewal left the deadline where it was"
                );
            }
            Err(error) => {
                let retry = self.defer(profile);
                tracing::warn!(
                    profile,
                    %error,
                    retry_in_s = retry.as_secs(),
                    "a credential could not be renewed; the sandbox failed"
                );
            }
        }
    }

    // The next moment this keeper has anything to do: the earliest deadline, less
    // the lead that makes renewing worth doing, and never sooner than a retry a
    // profile that failed is waiting on.
    fn next_wake(&self, profiles: &[String]) -> Instant {
        let now = Instant::now();
        let mut wake = now + LONGEST_SLEEP;
        for profile in profiles {
            if let Some(backoff) = self.backoff.get(profile) {
                if backoff.refused_at.is_none() {
                    wake = wake.min(backoff.retry_at);
                }
                continue;
            }
            if !self.credentials.can_renew(profile) {
                continue;
            }
            if let Some(expires_at_ms) = self.credentials.session_expiry(profile) {
                let due_in_ms = expires_at_ms - freshness::now_ms() - freshness::RENEW_WITHIN_MS;
                let at = now + Duration::from_millis(due_in_ms.max(0) as u64);
                wake = wake.min(at);
            }
        }
        wake
    }

    fn park(&mut self, profile: &str) {
        let seen = self.credentials.session_expiry(profile);
        self.backoff.insert(
            profile.to_owned(),
            Backoff {
                failures: 0,
                retry_at: Instant::now(),
                refused_at: Some(seen),
            },
        );
        let path = self.refusal_path(profile);
        let recorded = seen.map_or_else(|| "none".to_owned(), |at| at.to_string());
        if let Err(error) = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&path, recorded))
        {
            tracing::warn!(profile, %error, "could not record a refused renewal");
        }
    }

    // Beside this scope's data, not in the profile: a profile's files travel to every
    // sandbox that mounts it, and this is the keeper's own note.
    fn refusal_path(&self, profile: &str) -> PathBuf {
        self.credentials
            .config()
            .data_dir()
            .join("refused-renewals")
            .join(profile.replace('/', "__"))
    }

    // The session expiry a recorded refusal was given for, when there is one.
    fn recorded_refusal(&self, profile: &str) -> Option<Option<i64>> {
        let recorded = fs::read_to_string(self.refusal_path(profile)).ok()?;
        match recorded.trim() {
            "none" => Some(None),
            at => at.parse().ok().map(Some),
        }
    }

    fn defer(&mut self, profile: &str) -> Duration {
        let entry = self.backoff.entry(profile.to_owned()).or_insert(Backoff {
            failures: 0,
            retry_at: Instant::now(),
            refused_at: None,
        });
        entry.refused_at = None;
        entry.failures = entry.failures.saturating_add(1);
        let wait = FIRST_RETRY
            .saturating_mul(1u32 << (entry.failures.min(5) - 1))
            .min(LONGEST_RETRY);
        entry.retry_at = Instant::now() + wait;
        wait
    }
}

// A kept scope's whole clock: the first pass renews what is already due, and every
// pass after it sleeps to the next deadline.
pub async fn run<E>(credentials: Credentials<E>)
where
    E: IsolationEngine + Clone + 'static,
{
    let mut keeper = Keeper::new(credentials);
    loop {
        let wake = keeper.poll().await;
        sleep_until(wake).await;
    }
}

// The same rule as the keeper's, asked once for one profile: the caller is about to
// use it, which is the reason, and the profile's own deadline is the condition. The
// single-flight means a renewal already running is the one this waits for.
pub async fn renew_if_due<E>(credentials: &Credentials<E>, profile: &str)
where
    E: IsolationEngine + Clone + 'static,
{
    match credentials.renew_if_due(profile).await {
        Ok(Renewal::Renewed) => tracing::info!(profile, "the session is renewed"),
        Ok(Renewal::NotDue | Renewal::NothingToRenew) => {}
        Ok(Renewal::NoSession) => {
            tracing::warn!(profile, "a session that is due has no file to renew")
        }
        Ok(Renewal::Refused(reason)) => {
            tracing::warn!(profile, %reason, "a session that is due could not be renewed")
        }
        Ok(Renewal::Unchanged) => {
            tracing::warn!(
                profile,
                "a session that is due renewed to the same deadline"
            )
        }
        Err(error) => tracing::warn!(profile, %error, "a session that is due could not be renewed"),
    }
}

// The profiles this machine can renew: the ones whose directory actually holds a
// session. Read from the disk rather than from a settings list, so a machine renews
// what it has whether or not anything remembered to name it.
pub fn profiles(config: &Config) -> Vec<String> {
    let root = config.scoped_data_dir().to_path_buf();
    let mut found = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // A profile is a directory named "profile" holding a session, wherever in the
            // tree an operator's copy happens to sit: ids carry slashes.
            if path.file_name().is_some_and(|name| name == "profile") {
                if let Some(id) = path.parent().and_then(|dir| dir.strip_prefix(&root).ok()) {
                    found.push(id.to_string_lossy().into_owned());
                }
                continue;
            }
            pending.push(path);
        }
    }
    found
}
