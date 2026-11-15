use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

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

// The name a profile's OAuth session lives under, for the harnesses that keep one. A
// profile without this file carries no session to expire (a token in an environment
// file, say), and freshness has nothing to say about it.
pub const CREDENTIAL_FILE: &str = ".credentials.json";

const RENEW_WITHIN_MS: i64 = 15 * 60 * 1000;

#[derive(Deserialize)]
struct CredentialFile {
    #[serde(rename = "claudeAiOauth")]
    claude: Option<Oauth>,
}

#[derive(Deserialize)]
struct Oauth {
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "refreshTokenExpiresAt")]
    refresh_expires_at: Option<i64>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
}

// `None` says this profile has no session to expire, which is not a problem to report.
pub fn of_file(contents: &str) -> Option<Freshness> {
    let file: CredentialFile = serde_json::from_str(contents).ok()?;
    let oauth = file.claude?;
    let now = now_ms();
    let expires_at = oauth.expires_at?;
    let can_refresh = oauth
        .refresh_token
        .as_deref()
        .is_some_and(|token| !token.is_empty())
        && oauth.refresh_expires_at.is_none_or(|until| until > now);
    if expires_at > now + RENEW_WITHIN_MS {
        return Some(Freshness::Fresh {
            expires_at_ms: expires_at,
        });
    }
    if can_refresh {
        return Some(Freshness::RefreshDue {
            expires_at_ms: expires_at,
        });
    }
    Some(Freshness::Dead {
        since_ms: expires_at,
    })
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}
