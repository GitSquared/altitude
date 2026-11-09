use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{
    IssueStateChanged, StoredEvent, WEBHOOK_MAX_AGE,
    ledger::{CommentInput, CommentResolution, ReactionInput},
};

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

// A comment written on an issue is a comment whether or not Linear also thought to
// notify the app about it, so the comment data event is read as itself rather than
// waiting for a mention or a reply to something this machine published.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CommentEvent {
    pub(super) organization_id: String,
    pub(super) webhook_timestamp: f64,
    pub(super) action: String,
    pub(super) actor: IssueActor,
    pub(super) data: CommentData,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CommentData {
    pub(super) id: String,
    #[serde(default)]
    pub(super) body: String,
    #[serde(default)]
    pub(super) issue_id: Option<String>,
    #[serde(default)]
    pub(super) document_id: Option<String>,
    #[serde(default)]
    pub(super) user_id: Option<String>,
    // Whether the thread this comment belongs to is closed, and by whom. Absent means
    // the event did not speak about it; null means it was reopened.
    #[serde(default, deserialize_with = "present_or_null")]
    pub(super) resolved_at: Option<Option<String>>,
}

fn present_or_null<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

pub(super) fn parse_comment_event(body: &[u8]) -> Result<CommentEvent, serde_json::Error> {
    serde_json::from_slice(body)
}

impl CommentEvent {
    pub(super) fn is_fresh(&self) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        (now.as_millis() as f64 - self.webhook_timestamp).abs()
            <= WEBHOOK_MAX_AGE.as_millis() as f64
    }

    // A mention arrives as a notification as well, and either delivery names the same
    // comment, so which one comes first decides what is known about it: this one knows
    // only that something was said, and saying it is enough to admit it.
    pub(super) fn into_stored(self) -> Option<StoredEvent> {
        if self.actor.actor_type != "user" {
            return None;
        }
        // Resolving a thread is a comment event that says nothing new in the body: the
        // operator's word on whether the question is over, which the task reads as the
        // same close it would make in the interface.
        if let Some(resolved_at) = self.data.resolved_at {
            return Some(StoredEvent::CommentResolved(CommentResolution {
                comment_id: self.data.id,
                resolved: resolved_at.is_some(),
            }));
        }
        if self.action != "create" {
            return None;
        }
        let actor_id = self
            .data
            .user_id
            .filter(|id| !id.is_empty())
            .unwrap_or(self.actor.id);
        Some(StoredEvent::Comment(CommentInput {
            comment_id: self.data.id,
            issue_id: self.data.issue_id,
            document_id: self.data.document_id,
            actor_id,
            is_mention: false,
            body: self.data.body,
        }))
    }
}

// A reaction is a person answering with a symbol instead of a sentence. What it means
// is theirs to decide and the agent's to read in context, so it is carried to the
// thread as what happened rather than translated into an act.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReactionEvent {
    pub(super) organization_id: String,
    pub(super) webhook_timestamp: f64,
    pub(super) action: String,
    pub(super) actor: IssueActor,
    pub(super) data: ReactionData,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReactionData {
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) emoji: Option<String>,
    #[serde(default, alias = "reaction")]
    pub(super) reaction: Option<String>,
    #[serde(default)]
    pub(super) comment_id: Option<String>,
    #[serde(default)]
    pub(super) user_id: Option<String>,
}

pub(super) fn parse_reaction_event(body: &[u8]) -> Result<ReactionEvent, serde_json::Error> {
    serde_json::from_slice(body)
}

impl ReactionEvent {
    pub(super) fn is_fresh(&self) -> bool {
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        (now.as_millis() as f64 - self.webhook_timestamp).abs()
            <= WEBHOOK_MAX_AGE.as_millis() as f64
    }

    // Taking a reaction away is not a thing to say to the agent: it already read the
    // symbol, and a thread that says both is a thread arguing with itself.
    pub(super) fn into_stored(self) -> Option<StoredEvent> {
        if self.action != "create" || self.actor.actor_type != "user" {
            return None;
        }
        let emoji = self.data.emoji.or(self.data.reaction)?.trim().to_owned();
        let comment_id = self.data.comment_id?;
        let actor_id = self
            .data
            .user_id
            .filter(|id| !id.is_empty())
            .unwrap_or(self.actor.id);
        Some(StoredEvent::Reaction(ReactionInput {
            id: self
                .data
                .id
                .unwrap_or_else(|| format!("{comment_id}:{emoji}")),
            comment_id,
            emoji,
            actor_id,
        }))
    }
}
