use std::{cmp::Reverse, fs, io::ErrorKind, path::Path};

use serde::{Deserialize, Serialize};

use super::{DONE_RETENTION, MAX_DONE, MAX_TERMINATED, TERMINATED_RETENTION};
use crate::tracker::TrackerStatus;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Ledger {
    #[serde(default)]
    pub(super) deliveries: Vec<Delivery>,
    #[serde(default)]
    pub(super) cursors: Vec<Cursor>,
    #[serde(default)]
    pub(super) terminated_sessions: Vec<TerminatedSession>,
    #[serde(default)]
    pub(super) settled_issues: Vec<String>,
}

impl Ledger {
    pub(super) fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                format!(
                    "could not read the tracker ledger at {}: {error}",
                    path.display()
                )
            }),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!(
                "could not read the tracker ledger at {}: {error}",
                path.display()
            )),
        }
    }

    pub(super) fn prune(&mut self, now: i64) {
        self.prune_done(now);
        self.prune_terminated(now);
    }

    pub(super) fn prune_done(&mut self, now: i64) {
        let cutoff = now - DONE_RETENTION.as_millis() as i64;
        let mut pending = Vec::new();
        let mut done = Vec::new();
        for delivery in self.deliveries.drain(..) {
            if delivery.done {
                if delivery.received_at_ms >= cutoff {
                    done.push(delivery);
                }
            } else {
                pending.push(delivery);
            }
        }
        done.sort_by_key(|delivery| Reverse(delivery.received_at_ms));
        done.truncate(MAX_DONE);
        pending.extend(done);
        self.deliveries = pending;
    }

    pub(super) fn prune_terminated(&mut self, now: i64) {
        let cutoff = now - TERMINATED_RETENTION.as_millis() as i64;
        self.terminated_sessions
            .retain(|session| session.terminated_at_ms >= cutoff);
        if self.terminated_sessions.len() > MAX_TERMINATED {
            let excess = self.terminated_sessions.len() - MAX_TERMINATED;
            self.terminated_sessions.drain(..excess);
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Delivery {
    pub(super) key: String,
    pub(super) received_at_ms: i64,
    pub(super) next_attempt_at_ms: i64,
    pub(super) done: bool,
    pub(super) event: StoredEvent,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Cursor {
    pub(super) issue_id: String,
    #[serde(default)]
    pub(super) issue_identifier: String,
    pub(super) session: String,
    pub(super) owner: String,
    pub(super) tracker_user_id: String,
    pub(super) task_id: String,
    #[serde(default)]
    pub(super) reported_events: usize,
    #[serde(default)]
    pub(super) finished: bool,
    #[serde(default)]
    pub(super) failed_at: Option<usize>,
    #[serde(default)]
    pub(super) failed_attempts: usize,
    #[serde(default)]
    pub(super) last_status: Option<TrackerStatus>,
    #[serde(default)]
    pub(super) unhooked_at: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct TerminatedSession {
    pub(super) session_id: String,
    pub(super) terminated_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) enum StoredEvent {
    Session(Box<SessionEvent>),
    IssueStateChanged(IssueStateChanged),
    Comment(CommentInput),
}

impl StoredEvent {
    pub(super) fn describe(&self) -> String {
        match self {
            StoredEvent::Session(event) => {
                format!("session {} · {}", event.session_id, event.action)
            }
            StoredEvent::IssueStateChanged(change) => {
                format!("issue {} · state {}", change.issue_id, change.state_id)
            }
            StoredEvent::Comment(comment) => {
                format!(
                    "comment {} · {}",
                    comment.comment_id,
                    if comment.is_mention {
                        "mention"
                    } else {
                        "comment"
                    }
                )
            }
        }
    }

    pub(super) fn dedupe_key(&self) -> String {
        match self {
            StoredEvent::Session(session) => session.dedupe_key(),
            StoredEvent::IssueStateChanged(change) => format!(
                "issue:{}:{}:{}:{}",
                change.issue_id, change.state_id, change.actor_id, change.occurred_at_ms
            ),
            StoredEvent::Comment(comment) => format!("comment:{}", comment.comment_id),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct SessionEvent {
    pub(super) action: String,
    pub(super) session_id: String,
    #[serde(default)]
    pub(super) issue_id: Option<String>,
    #[serde(default)]
    pub(super) comment_id: Option<String>,
    #[serde(default)]
    pub(super) creator_id: Option<String>,
    #[serde(default)]
    pub(super) creator_name: Option<String>,
    #[serde(default)]
    pub(super) prompt_author_id: Option<String>,
    #[serde(default)]
    pub(super) prompt_body: Option<String>,
    #[serde(default)]
    pub(super) signal: Option<String>,
    #[serde(default)]
    pub(super) source_comment_id: Option<String>,
    #[serde(default)]
    pub(super) activity_id: Option<String>,
    #[serde(default)]
    pub(super) occurred_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct IssueStateChanged {
    pub(super) issue_id: String,
    pub(super) state_id: String,
    pub(super) actor_id: String,
    pub(super) occurred_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct CommentInput {
    pub(super) comment_id: String,
    #[serde(default)]
    pub(super) issue_id: Option<String>,
    #[serde(default)]
    pub(super) document_id: Option<String>,
    pub(super) actor_id: String,
    pub(super) is_mention: bool,
    pub(super) body: String,
}
