use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use super::GitHubIntake;
use crate::tracker::host::remote::ledger::{
    CommentInput, IssueStateChanged, SessionEvent, StoredEvent,
};
use crate::tracker::host::remote::now_ms;
use crate::tracker::providers::github::automation::{
    session_id,
    webhook::{
        DELIVERY_HEADER, EVENT_HEADER, IssueChange, SIGNATURE_HEADER, read_change, verify_signature,
    },
};

// GitHub's events are translated into the ones the ledger already keeps, so what
// happens after a delivery is the same machinery Linear's go through: a label is a
// delegation, a comment on an issue with a session is a prompt in it, closing an
// issue is a state change.
pub(super) async fn webhook(
    State(intake): State<Arc<GitHubIntake>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let Some(signature) = header(SIGNATURE_HEADER) else {
        tracing::warn!("GitHub delivery rejected · the request carried no signature header");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !verify_signature(&intake.webhook_secret, Some(signature), &body) {
        tracing::warn!(
            "GitHub delivery rejected · its signature does not match this machine's webhook secret"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let event = header(EVENT_HEADER).unwrap_or_default();
    if event == "ping" {
        tracing::info!("GitHub said hello · the app's webhook reaches this machine");
        return StatusCode::OK.into_response();
    }
    let Some(delivery) = header(DELIVERY_HEADER).filter(|id| !id.is_empty()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let change = match read_change(event, &body) {
        Ok(Some(change)) => change,
        Ok(None) => {
            tracing::debug!(event, "GitHub delivery ignored");
            return StatusCode::OK.into_response();
        }
        Err(reason) => {
            tracing::warn!(%reason, "GitHub delivery rejected");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // A label taken off has nothing to store: whether the issue is still delegated is
    // read from the issue on the next pass, the same way Linear's is, and this only
    // makes that pass happen now.
    let Some(stored) = intake.stored_event(change, delivery) else {
        intake.tracker.wake();
        return StatusCode::OK.into_response();
    };
    let described = stored.describe();
    match intake.tracker.enqueue(stored.dedupe_key(), stored) {
        Ok(true) => tracing::info!(delivery = %described, "GitHub delivery recorded"),
        Ok(false) => tracing::debug!(delivery = %described, "GitHub delivery was already recorded"),
        Err(error) => {
            tracing::error!(%error, "could not durably record a GitHub delivery");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    StatusCode::OK.into_response()
}

impl GitHubIntake {
    fn stored_event(&self, change: IssueChange, delivery: &str) -> Option<StoredEvent> {
        match change {
            // The delivery names the delegation, so GitHub sending the same one again
            // is the same session rather than a second delegation replacing the first.
            IssueChange::Delegated {
                issue,
                delegator_id,
                delegator_login,
            } => Some(StoredEvent::Session(Box::new(SessionEvent {
                action: "created".to_owned(),
                session_id: session_id(&issue, delivery),
                issue_id: Some(issue.id()),
                creator_id: Some(delegator_id),
                creator_name: Some(delegator_login),
                occurred_at_ms: now_ms(),
                ..SessionEvent::default()
            }))),
            IssueChange::Withdrawn => None,
            IssueChange::StateChanged {
                issue,
                state_id,
                actor_id,
            } => Some(StoredEvent::IssueStateChanged(IssueStateChanged {
                issue_id: issue.id(),
                state_id,
                actor_id,
                occurred_at_ms: now_ms(),
            })),
            IssueChange::Commented {
                comment,
                author_id,
                body,
                written_at_ms,
            } => {
                // This app's own comments come back as deliveries too, and nobody is
                // speaking in them.
                if author_id == self.app_user_id {
                    return None;
                }
                let issue_id = comment.issue.id();
                // GitHub's comments are one list with no threads, so the issue's
                // session is the conversation a comment is part of.
                match self.tracker.session_for_issue(&issue_id) {
                    Some(session) => Some(StoredEvent::Session(Box::new(SessionEvent {
                        action: "prompted".to_owned(),
                        session_id: session,
                        issue_id: Some(issue_id),
                        prompt_author_id: Some(author_id),
                        prompt_body: Some(body),
                        activity_id: Some(comment.id()),
                        occurred_at_ms: written_at_ms,
                        ..SessionEvent::default()
                    }))),
                    None => Some(StoredEvent::Comment(CommentInput {
                        comment_id: comment.id(),
                        issue_id: Some(issue_id),
                        document_id: None,
                        actor_id: author_id,
                        is_mention: false,
                        body,
                    })),
                }
            }
        }
    }
}
