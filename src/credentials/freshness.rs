use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::SessionExpiry;

// How long the credential in a profile will keep working, read from the profile's own
// files rather than remembered anywhere. A fleet that has been idle for a week therefore
// says the same thing as one that has just synced, and nobody has to guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    // Fresh enough that a task starting now will not be the thing that spends it.
    Fresh { expires_at_ms: i64 },
    // The access token is spent or nearly spent, but the profile can buy another one the
    // first time something uses it. Worth saying, not worth refusing.
    RefreshDue { expires_at_ms: i64 },
    // Nothing in this profile can renew it: only somebody signing in can.
    Dead { since_ms: i64 },
}

// A session with less than this left is worth renewing now rather than mid-task.
const RENEW_WITHIN_MS: i64 = 15 * 60 * 1000;

// Where a session stands, worked out from what its own provider read out of its own file.
// Nothing here knows what a credential looks like.
pub fn of(session: SessionExpiry) -> Freshness {
    let now = now_ms();
    if session.expires_at_ms > now + RENEW_WITHIN_MS {
        return Freshness::Fresh {
            expires_at_ms: session.expires_at_ms,
        };
    }
    if session.can_renew {
        return Freshness::RefreshDue {
            expires_at_ms: session.expires_at_ms,
        };
    }
    Freshness::Dead {
        since_ms: session.expires_at_ms,
    }
}

impl Freshness {
    pub fn describe(self) -> String {
        match self {
            Self::Fresh { expires_at_ms } => {
                format!("expires in {}", span(expires_at_ms - now_ms()))
            }
            Self::RefreshDue { expires_at_ms } => format!(
                "spent {} ago; renews when a sandbox next uses it",
                span(now_ms() - expires_at_ms)
            ),
            Self::Dead { since_ms } => format!(
                "expired {} ago and cannot renew itself",
                span(now_ms() - since_ms)
            ),
        }
    }

    pub fn is_dead(self) -> bool {
        matches!(self, Self::Dead { .. })
    }
}

fn span(millis: i64) -> String {
    let minutes = millis / 60_000;
    match minutes {
        m if m < 1 => "less than a minute".to_owned(),
        m if m < 60 => format!("{m}m"),
        m if m < 60 * 48 => format!("{}h", m / 60),
        m => format!("{}d", m / (60 * 24)),
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}
