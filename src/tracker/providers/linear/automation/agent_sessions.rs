use std::{
    error::Error,
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::tracker::IssueReference;

use super::queries::{AGENT_SESSION_EVENT, WEBHOOK_MAX_AGE};

#[derive(Clone, Debug)]
pub struct DelegatedIssue {
    pub reference: IssueReference,
    pub session: Option<String>,
    // When the operator made the delegation: how old the newest session is. A
    // delegation nobody picked up while it was news is not picked up on a boot
    // sweep days later.
    pub delegated_at_ms: Option<i64>,
}

pub struct IssueDelegation {
    pub delegate_id: Option<String>,
    pub state: WorkflowState,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowState {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub state_type: String,
}

#[derive(Clone, Debug)]
pub struct CommentSite {
    pub comment_id: String,
    pub body: String,
    pub parent_id: Option<String>,
    pub issue_id: Option<String>,
    pub document_content_id: Option<String>,
    pub document_id: Option<String>,
    pub root_comment_id: String,
    pub root_author_id: Option<String>,
    pub document_creator_id: Option<String>,
    pub author_name: Option<String>,
    pub is_artificial_session_root: bool,
    pub authors_truncated: bool,
    pub human_authors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AgentActivityContent {
    Thought { body: String },
    Response { body: String },
    Error { body: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub action: String,
    pub organization_id: String,
    pub app_user_id: String,
    pub oauth_client_id: String,
    pub webhook_id: String,
    pub webhook_timestamp: f64,
    pub agent_session: AgentSession,
    pub agent_activity: Option<AgentActivityPayload>,
}

impl AgentSessionEvent {
    pub fn validate(&self, workspace_id: &str, app_user_id: &str) -> Result<(), WebhookError> {
        if self.event_type != AGENT_SESSION_EVENT {
            return Err(WebhookError::UnsupportedEvent(self.event_type.clone()));
        }
        if !self.timestamp_is_fresh() {
            return Err(WebhookError::StaleTimestamp);
        }
        if self.organization_id != workspace_id
            || self.agent_session.organization_id != workspace_id
        {
            return Err(WebhookError::ForeignWorkspace);
        }
        if self.app_user_id != app_user_id || self.agent_session.app_user_id != app_user_id {
            return Err(WebhookError::ForeignApp);
        }
        Ok(())
    }

    pub fn delegation_author_id(&self) -> Option<&str> {
        self.agent_session.creator_id.as_deref()
    }

    pub fn prompt_author_id(&self) -> Option<&str> {
        self.agent_activity
            .as_ref()
            .map(|activity| activity.user_id.as_str())
    }

    fn timestamp_is_fresh(&self) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        let age = (now.as_millis() as f64 - self.webhook_timestamp).abs();
        age <= WEBHOOK_MAX_AGE.as_millis() as f64
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSession {
    pub id: String,
    pub app_user_id: String,
    pub organization_id: String,
    pub creator_id: Option<String>,
    pub creator: Option<WebhookUser>,
    pub issue_id: Option<String>,
    pub comment_id: Option<String>,
    pub source_comment_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentActivityPayload {
    pub id: String,
    pub user_id: String,
    pub user: WebhookUser,
    pub content: WebhookActivityContent,
    pub signal: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookActivityContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub body: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookUser {
    pub id: String,
    pub name: String,
}

#[derive(Debug)]
pub enum WebhookError {
    StaleTimestamp,
    UnsupportedEvent(String),
    ForeignWorkspace,
    ForeignApp,
    Malformed(serde_json::Error),
}

impl fmt::Display for WebhookError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleTimestamp => formatter.write_str("the Linear webhook is stale"),
            Self::UnsupportedEvent(event) => {
                write!(formatter, "unsupported Linear webhook event: {event}")
            }
            Self::ForeignWorkspace => {
                formatter.write_str("the Linear webhook belongs to a different workspace")
            }
            Self::ForeignApp => {
                formatter.write_str("the Linear webhook belongs to a different app user")
            }
            Self::Malformed(error) => {
                write!(formatter, "the Linear webhook body is malformed: {error}")
            }
        }
    }
}

impl Error for WebhookError {}

pub fn verify_signature(secret: &[u8], signature: Option<&str>, body: &[u8]) -> bool {
    let Some(signature) = signature.and_then(decode_signature) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

pub fn parse_agent_session_event(body: &[u8]) -> Result<AgentSessionEvent, WebhookError> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type")]
        event_type: String,
    }
    let envelope: Envelope = serde_json::from_slice(body).map_err(WebhookError::Malformed)?;
    if envelope.event_type != AGENT_SESSION_EVENT {
        return Err(WebhookError::UnsupportedEvent(envelope.event_type));
    }
    serde_json::from_slice(body).map_err(WebhookError::Malformed)
}

fn decode_signature(value: &str) -> Option<Vec<u8>> {
    // Linear sends 32 bytes as lowercase hex; reject anything else before the
    // MAC is built so a short or non-hex header cannot reach verify_slice.
    if value.len() != 64 {
        return None;
    }
    hex::decode(value).ok()
}
