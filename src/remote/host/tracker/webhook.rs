use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{IssueStateChanged, StoredEvent, WEBHOOK_MAX_AGE, ledger::CommentInput};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IssueEvent {
    pub(super) organization_id: String,
    pub(super) webhook_timestamp: f64,
    pub(super) created_at: String,
    pub(super) actor: IssueActor,
    pub(super) data: IssueData,
    #[serde(default)]
    pub(super) updated_from: Option<IssueUpdatedFrom>,
}

#[derive(Clone, Deserialize)]
pub(super) struct IssueActor {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) actor_type: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IssueData {
    pub(super) id: String,
    #[serde(default)]
    pub(super) state_id: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IssueUpdatedFrom {
    #[serde(default)]
    pub(super) state_id: Option<String>,
}

pub(super) fn parse_issue_event(body: &[u8]) -> Result<IssueEvent, serde_json::Error> {
    serde_json::from_slice(body)
}

impl IssueEvent {
    pub(super) fn is_fresh(&self) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        (now.as_millis() as f64 - self.webhook_timestamp).abs()
            <= WEBHOOK_MAX_AGE.as_millis() as f64
    }

    pub(super) fn into_stored(self) -> Option<StoredEvent> {
        if self.actor.actor_type != "user" {
            return None;
        }
        let state_id = self.data.state_id?;
        let previous_state_id = self.updated_from?.state_id?;
        if previous_state_id == state_id {
            return None;
        }
        let occurred_at_ms = chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .ok()?
            .timestamp_millis();
        Some(StoredEvent::IssueStateChanged(IssueStateChanged {
            issue_id: self.data.id,
            state_id,
            actor_id: self.actor.id,
            occurred_at_ms,
        }))
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AppNotificationEvent {
    pub(super) organization_id: String,
    pub(super) app_user_id: String,
    #[serde(default)]
    pub(super) oauth_client_id: Option<String>,
    pub(super) webhook_timestamp: f64,
    pub(super) action: String,
    pub(super) notification: AppNotification,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AppNotification {
    #[serde(rename = "type")]
    pub(super) notification_type: String,
    #[serde(default)]
    pub(super) actor_id: Option<String>,
    #[serde(default)]
    pub(super) comment_id: Option<String>,
    #[serde(default)]
    pub(super) issue_id: Option<String>,
    #[serde(default)]
    pub(super) document_id: Option<String>,
    #[serde(default)]
    pub(super) comment: Option<NotificationComment>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NotificationComment {
    #[serde(default)]
    pub(super) id: String,
    #[serde(default)]
    pub(super) body: String,
    #[serde(default)]
    pub(super) user_id: Option<String>,
    #[serde(default)]
    pub(super) issue_id: Option<String>,
}

pub(super) fn parse_app_notification(
    body: &[u8],
) -> Result<AppNotificationEvent, serde_json::Error> {
    serde_json::from_slice(body)
}

impl AppNotificationEvent {
    pub(super) fn is_fresh(&self) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        (now.as_millis() as f64 - self.webhook_timestamp).abs()
            <= WEBHOOK_MAX_AGE.as_millis() as f64
    }

    pub(super) fn into_stored(self) -> Option<StoredEvent> {
        let is_mention = matches!(
            self.action.as_str(),
            "issueCommentMention" | "documentCommentMention"
        ) || self.notification.notification_type.ends_with("Mention");
        let is_new_comment = matches!(
            self.action.as_str(),
            "issueNewComment" | "documentNewComment"
        );
        if !is_mention && !is_new_comment {
            return None;
        }
        let comment = self.notification.comment.unwrap_or_default();
        let comment_id = self
            .notification
            .comment_id
            .or_else(|| (!comment.id.is_empty()).then_some(comment.id))?;
        let actor_id = self
            .notification
            .actor_id
            .or(comment.user_id)
            .filter(|id| !id.is_empty())?;
        let issue_id = self.notification.issue_id.or(comment.issue_id);
        Some(StoredEvent::Comment(CommentInput {
            comment_id,
            issue_id,
            document_id: self.notification.document_id,
            actor_id,
            is_mention,
            body: comment.body,
        }))
    }
}
