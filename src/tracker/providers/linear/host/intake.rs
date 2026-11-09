use super::ledger::{SessionEvent, StoredEvent};
use super::webhook::{
    parse_app_notification, parse_comment_event, parse_issue_event, parse_reaction_event,
};
use crate::tracker::providers::linear::automation::{
    AgentSessionEvent, WebhookError, parse_agent_session_event, verify_signature,
};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::{Arc, atomic::Ordering};

use super::*;

pub(super) enum WebhookOutcome {
    Store(StoredEvent),
    Ignore(&'static str),
    Reject(String),
}

pub(super) async fn webhook(
    State(tracker): State<Arc<RemoteTracker>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok());
    // A delivery with no signature is almost never Linear: anything that finds the
    // public address can post here. Saying which of the two it was keeps a probe
    // from reading as a secret that needs rotating.
    let Some(signature) = signature else {
        tracing::warn!("Linear delivery rejected · the request carried no signature header");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !verify_signature(&tracker.config.webhook_secret, Some(signature), &body) {
        // Loud, and without the value: a secret that does not match the app's is
        // the one misconfiguration that otherwise looks like Linear sending
        // nothing at all.
        tracing::warn!(
            "Linear delivery rejected · its signature does not match this machine's webhook secret"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let event = match classify_webhook(&tracker, &body) {
        WebhookOutcome::Store(event) => event,
        // A well-formed webhook for another resource is not this coordinator's
        // work. Acknowledging it stops Linear retrying it forever.
        WebhookOutcome::Ignore(reason) => {
            tracing::debug!(reason, "Linear delivery ignored");
            return StatusCode::OK.into_response();
        }
        WebhookOutcome::Reject(reason) => {
            tracing::warn!(%reason, "Linear delivery rejected");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // The line says what arrived, never what it said: a delivery carries the
    // operator's own words, and a log is not the place to keep them.
    let delivery = event.describe();
    // Identity comes from the signed body, not the unsigned delivery header:
    // a header a sender can choose is transport, not what the event is.
    let key = event.dedupe_key();
    match tracker.enqueue(key, event) {
        Ok(true) if !tracker.delivered.swap(true, Ordering::Relaxed) => tracing::info!(
            %delivery,
            "Linear delivery received · the first one since this machine started"
        ),
        Ok(true) => tracing::debug!(%delivery, "Linear delivery recorded"),
        Ok(false) => tracing::debug!(%delivery, "Linear delivery was already recorded"),
        Err(error) => {
            tracing::error!(%error, "could not durably record a Linear delivery");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    StatusCode::OK.into_response()
}

fn classify_webhook(tracker: &RemoteTracker, body: &[u8]) -> WebhookOutcome {
    let Some(event_type) = webhook_event_type(body) else {
        return WebhookOutcome::Reject("the body has no event type".to_owned());
    };
    match event_type.as_str() {
        "AgentSessionEvent" => match parse_agent_session_event(body) {
            Err(WebhookError::UnsupportedEvent(_)) => {
                WebhookOutcome::Ignore("an agent session action this coordinator does not run")
            }
            Err(error) => WebhookOutcome::Reject(format!(
                "the agent session event could not be read: {error}"
            )),
            Ok(event) => {
                if event
                    .validate(
                        tracker.config.automation.workspace_id(),
                        tracker.config.automation.app_user_id(),
                    )
                    .is_err()
                {
                    return WebhookOutcome::Reject("another workspace or app".to_owned());
                }
                if event.oauth_client_id != tracker.config.oauth_client_id {
                    return WebhookOutcome::Reject("another OAuth application".to_owned());
                }
                WebhookOutcome::Store(StoredEvent::Session(Box::new(SessionEvent::from(&event))))
            }
        },
        "Issue" => match parse_issue_event(body) {
            Err(error) => {
                WebhookOutcome::Reject(format!("the issue event could not be read: {error}"))
            }
            Ok(event) => {
                if event.organization_id != tracker.config.automation.workspace_id() {
                    return WebhookOutcome::Reject("another workspace".to_owned());
                }
                if !event.is_fresh() {
                    return WebhookOutcome::Reject(
                        "older than the freshness window · check this machine's clock".to_owned(),
                    );
                }
                match event.into_stored() {
                    Some(stored) => WebhookOutcome::Store(stored),
                    None => WebhookOutcome::Ignore("not a state change a person made"),
                }
            }
        },
        "Comment" => match parse_comment_event(body) {
            Err(error) => {
                WebhookOutcome::Reject(format!("the comment event could not be read: {error}"))
            }
            Ok(event) => {
                if event.organization_id != tracker.config.automation.workspace_id() {
                    return WebhookOutcome::Reject("another workspace".to_owned());
                }
                if !event.is_fresh() {
                    return WebhookOutcome::Reject(
                        "older than the freshness window · check this machine's clock".to_owned(),
                    );
                }
                match event.into_stored() {
                    Some(stored) => WebhookOutcome::Store(stored),
                    None => WebhookOutcome::Ignore("not a comment someone wrote"),
                }
            }
        },
        "Reaction" => match parse_reaction_event(body) {
            Err(error) => {
                WebhookOutcome::Reject(format!("the reaction event could not be read: {error}"))
            }
            Ok(event) => {
                if event.organization_id != tracker.config.automation.workspace_id() {
                    return WebhookOutcome::Reject("another workspace".to_owned());
                }
                if !event.is_fresh() {
                    return WebhookOutcome::Reject(
                        "older than the freshness window · check this machine's clock".to_owned(),
                    );
                }
                match event.into_stored() {
                    Some(stored) => WebhookOutcome::Store(stored),
                    None => WebhookOutcome::Ignore("not a reaction someone added"),
                }
            }
        },
        "AppUserNotification" => match parse_app_notification(body) {
            Err(error) => {
                WebhookOutcome::Reject(format!("the notification event could not be read: {error}"))
            }
            Ok(event) => {
                if event.organization_id != tracker.config.automation.workspace_id() {
                    return WebhookOutcome::Reject("another workspace".to_owned());
                }
                if event.app_user_id != tracker.config.automation.app_user_id() {
                    return WebhookOutcome::Reject("another app user".to_owned());
                }
                if event
                    .oauth_client_id
                    .as_deref()
                    .is_some_and(|id| id != tracker.config.oauth_client_id)
                {
                    return WebhookOutcome::Reject("another OAuth application".to_owned());
                }
                if !event.is_fresh() {
                    return WebhookOutcome::Reject(
                        "older than the freshness window · check this machine's clock".to_owned(),
                    );
                }
                match event.into_stored() {
                    Some(stored) => WebhookOutcome::Store(stored),
                    None => WebhookOutcome::Ignore("a notification that carries no comment"),
                }
            }
        },
        _ => WebhookOutcome::Ignore("not a resource this coordinator handles"),
    }
}

fn webhook_event_type(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type")]
        event_type: String,
    }
    serde_json::from_slice::<Envelope>(body)
        .ok()
        .map(|envelope| envelope.event_type)
}

impl SessionEvent {
    pub(super) fn is_stop(&self) -> bool {
        if self.signal.as_deref() == Some("stop") {
            return true;
        }
        let action = self.action.to_ascii_lowercase();
        if [
            "cancel",
            "canceled",
            "cancelled",
            "stop",
            "stopped",
            "abort",
            "aborted",
        ]
        .contains(&action.as_str())
        {
            return true;
        }
        if let Some(body) = self.prompt_body.as_deref() {
            let body = body.trim().to_ascii_lowercase();
            if ["stop", "cancel", "cancelled", "canceled"].contains(&body.as_str()) {
                return true;
            }
        }
        false
    }

    pub(super) fn is_from(&self, tracker_user_id: &str) -> bool {
        self.prompt_author_id.as_deref() == Some(tracker_user_id)
    }

    pub(super) fn dedupe_key(&self) -> String {
        if self.is_stop() {
            return format!(
                "session:{}:stop:{}",
                self.session_id,
                self.activity_id.as_deref().unwrap_or_default()
            );
        }
        match self.action.as_str() {
            "created" => format!("session:{}:created", self.session_id),
            "prompted" => match &self.activity_id {
                Some(activity_id) => format!("activity:{activity_id}"),
                None => format!(
                    "prompt:{}:{}",
                    self.session_id,
                    self.prompt_body.as_deref().unwrap_or_default()
                ),
            },
            other => format!("session:{}:{other}", self.session_id),
        }
    }
}

impl From<&AgentSessionEvent> for SessionEvent {
    fn from(event: &AgentSessionEvent) -> Self {
        let activity = event.agent_activity.as_ref();
        Self {
            action: event.action.clone(),
            session_id: event.agent_session.id.clone(),
            issue_id: event.agent_session.issue_id.clone(),
            comment_id: event.agent_session.comment_id.clone(),
            creator_id: event.delegation_author_id().map(str::to_owned),
            creator_name: event
                .agent_session
                .creator
                .as_ref()
                .map(|creator| creator.name.clone()),
            prompt_author_id: event.prompt_author_id().map(str::to_owned),
            prompt_body: activity.and_then(|activity| activity.content.body.clone()),
            signal: activity.and_then(|activity| activity.signal.clone()),
            source_comment_id: event.agent_session.source_comment_id.clone(),
            activity_id: activity.map(|activity| activity.id.clone()),
            occurred_at_ms: event.webhook_timestamp as i64,
        }
    }
}
