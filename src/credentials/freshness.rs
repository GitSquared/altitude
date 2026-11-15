use std::time::{SystemTime, UNIX_EPOCH};

// A session with less than this left is worth renewing now rather than mid-task.
pub const RENEW_WITHIN_MS: i64 = 15 * 60 * 1000;

// Where a session stands, as the two facts its own provider read out of its own files:
// when it stops working, and whether using it can extend that. Nothing here classifies a
// session into a named state — every question asked of it is a comparison, so a provider
// that reports different facts gets sensible answers without this layer learning about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
    pub expires_at_ms: i64,
}

impl Standing {
    pub fn new(expires_at_ms: i64) -> Self {
        Self { expires_at_ms }
    }

    // Whether acting now is worth it: spent, or close enough that a task starting would
    // be the thing that spends it.
    pub fn is_due(self) -> bool {
        self.expires_at_ms <= now_ms() + RENEW_WITHIN_MS
    }

    // Whether this copy will outlast another, which is the whole of the rule for deciding
    // which of two copies of one credential to keep.
    pub fn outlasts(self, other: Self) -> bool {
        self.expires_at_ms > other.expires_at_ms
    }

    // Said in the terms the reader needs. Whether a spent session can come back is the
    // provider's answer, not this one's: `renewable` is passed in by whoever asked it.
    pub fn describe(self, renewable: bool) -> String {
        let left = self.expires_at_ms - now_ms();
        match (left >= 0, renewable) {
            (true, _) => format!("expires in {}", span(left)),
            (false, true) => format!(
                "spent {} ago; renews when a sandbox next uses it",
                span(-left)
            ),
            (false, false) => format!("expired {} ago and cannot renew itself", span(-left)),
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
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
