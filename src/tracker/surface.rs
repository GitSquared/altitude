use crate::tracker::{IssueReference, TrackerFuture};

// A place a task's asks can be put to a person and closed again. A tracker with
// threaded comments answers with a thread; one with a flat feed answers with a
// marker in the body; one with neither answers by doing nothing. The host asks in
// the same words everywhere and maps what it gets back to the task's own threads.
pub trait AskSurface: Send + Sync {
    // `marker` is the discussion's identity, stable across retries, so opening the
    // same ask twice is one discussion rather than two.
    fn open_ask<'a>(
        &'a self,
        issue: &'a IssueReference,
        marker: &'a str,
        body: &'a str,
    ) -> TrackerFuture<'a, ()>;

    // The agent's answer goes where the ask was put. `reply_marker` is the reply's
    // own stable identity, for the same reason.
    fn answer_ask<'a>(
        &'a self,
        issue: &'a IssueReference,
        marker: &'a str,
        reply_marker: &'a str,
        body: &'a str,
    ) -> TrackerFuture<'a, ()>;

    // Closing is the operator's word that the question is over. A surface without
    // the idea keeps the ask where it is and returns success: there is nothing to
    // do, not something that failed.
    fn close_ask<'a>(&'a self, _marker: &'a str, _closed: bool) -> TrackerFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    // Whether this machine ever opened the discussion. A reply into a discussion
    // that does not exist is not an answer, and a close that finds nothing is not
    // an error.
    fn has_ask<'a>(&'a self, marker: &'a str) -> TrackerFuture<'a, bool>;
}

// The one place ask identity is derived. The task and the signal are what both
// sides know, so a provider that cannot store the marker as an id can write it
// into the body and a reader can still find the discussion.
pub fn ask_marker(task_id: &str, signal_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(task_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(signal_id.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    // Linear requires a v4 UUID for a comment id; providers that treat the marker
    // as text keep the same shape rather than growing a second convention.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}
