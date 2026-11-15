use std::{fs, time::Duration};

use crate::{config::Config, isolation::IsolationEngine};

use super::freshness::{self, Freshness};
use super::{Credentials, Renewal};

// A session is kept alive by the only thing that can keep it alive: using it. The check
// that asks a profile whether it still signs in runs the harness in a sandbox, and the
// harness renews its own session while it is there — so a profile that is close to
// running out is renewed by asking.
const RENEW_WITHIN: Duration = Duration::from_secs(60 * 60);

// How often a running machine looks. Nothing is refreshed because this fired; it fired
// to look at what the profile's own files say.
pub const LOOK_EVERY: Duration = Duration::from_secs(15 * 60);

pub fn due(credentials: &Credentials<impl IsolationEngine>, profile: &str) -> Option<Freshness> {
    let contents = fs::read_to_string(
        credentials
            .config()
            .profile_dir(profile)
            .join(freshness::CREDENTIAL_FILE),
    )
    .ok()?;
    match freshness::of_file(&contents)? {
        Freshness::Fresh { expires_at_ms }
            if expires_at_ms > freshness::now_ms() + RENEW_WITHIN.as_millis() as i64 =>
        {
            None
        }
        due => Some(due),
    }
}

// The profiles this machine can renew: the ones whose directory actually holds a session.
// Read from the disk rather than from a settings list, so a machine renews what it has
// whether or not anything remembered to name it.
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
                if path.join(freshness::CREDENTIAL_FILE).is_file()
                    && let Some(id) = path.parent().and_then(|dir| dir.strip_prefix(&root).ok())
                {
                    found.push(id.to_string_lossy().into_owned());
                }
                continue;
            }
            pending.push(path);
        }
    }
    found
}

// One pass over the profiles this machine holds. A profile that is not due is not
// touched, so a healthy machine costs a read per profile and nothing else.
pub async fn refresh_due<E>(credentials: &Credentials<E>, profiles: &[String])
where
    E: IsolationEngine + Clone + 'static,
{
    for profile in profiles {
        // Which of these are worth refreshing is decided by the session's own clock, not
        // by this one: `due` returns nothing for a profile that has hours left.
        let Some(state) = due(credentials, profile) else {
            continue;
        };
        tracing::info!(profile, "{}; renewing it", state.describe());
        match credentials.renew(profile).await {
            Ok(Renewal::Renewed) => tracing::info!(profile, "renewed"),
            Ok(Renewal::NothingToRenew) => {
                tracing::info!(profile, "nothing in this profile renews itself")
            }
            Ok(Renewal::Refused(output)) => tracing::warn!(
                profile,
                %output,
                "could not renew this credential; it needs a sign-in"
            ),
            Err(error) => tracing::warn!(profile, %error, "could not renew this credential"),
        }
    }
}
