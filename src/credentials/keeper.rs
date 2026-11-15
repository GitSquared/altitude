use std::{fs, time::Duration};

use crate::isolation::IsolationEngine;

use super::freshness::{self, Freshness};
use super::{Credentials, Renewed};

// A session is kept alive by the only thing that can keep it alive: using it. The check
// that asks a profile whether it still signs in runs the harness in a sandbox, and the
// harness renews its own session while it is there — so a profile that is close to
// running out is renewed by asking.
const RENEW_WITHIN: Duration = Duration::from_secs(60 * 60);

// How often a running machine looks. Nothing is refreshed because this fired; it fired
// to look at what the profile's own files say.
pub const LOOK_EVERY: Duration = Duration::from_secs(15 * 60);

pub fn due(credentials: &Credentials<impl IsolationEngine>, profile: &str) -> Option<Freshness> {
    let path = credentials
        .config()
        .data_dir()
        .join(profile)
        .join("profile")
        .join(freshness::CREDENTIAL_FILE);
    let contents = fs::read_to_string(path).ok()?;
    match freshness::of_file(&contents)? {
        Freshness::Fresh { expires_at_ms }
            if expires_at_ms > freshness::now_ms() + RENEW_WITHIN.as_millis() as i64 =>
        {
            None
        }
        due => Some(due),
    }
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
            Ok(Renewed::Renewed) => tracing::info!(profile, "renewed"),
            Ok(Renewed::NothingToRenew) => {
                tracing::info!(profile, "nothing in this profile renews itself")
            }
            Ok(Renewed::Refused(output)) => tracing::warn!(
                profile,
                %output,
                "could not renew this credential; it needs a sign-in"
            ),
            Err(error) => tracing::warn!(profile, %error, "could not renew this credential"),
        }
    }
}
