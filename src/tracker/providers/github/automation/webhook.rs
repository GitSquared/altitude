use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;

use super::super::issues::IssueAddress;
use super::{DELEGATION_LABEL, WrittenComment, workflow_state};

pub(crate) const SIGNATURE_HEADER: &str = "x-hub-signature-256";
pub(crate) const EVENT_HEADER: &str = "x-github-event";
pub(crate) const DELIVERY_HEADER: &str = "x-github-delivery";

// What happened on an issue, in the terms the remote acts on. Everything else GitHub
// sends an app is ignored before it gets this far.
pub(crate) enum IssueChange {
    Delegated {
        issue: IssueAddress,
        delegator_id: String,
        delegator_login: String,
    },
    Withdrawn,
    StateChanged {
        issue: IssueAddress,
        state_id: String,
        actor_id: String,
    },
    Commented {
        comment: WrittenComment,
        author_id: String,
        body: String,
        written_at_ms: i64,
    },
}

#[derive(Deserialize)]
struct Payload {
    action: String,
    issue: Issue,
    sender: Account,
    #[serde(default)]
    label: Option<Label>,
    #[serde(default)]
    comment: Option<Comment>,
}

#[derive(Deserialize)]
struct Issue {
    html_url: String,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
    #[serde(flatten)]
    rest: serde_json::Value,
}

#[derive(Deserialize)]
struct Account {
    id: u64,
    login: String,
    #[serde(rename = "type")]
    account_type: String,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct Comment {
    id: u64,
    #[serde(default)]
    body: String,
    created_at: String,
}

pub(crate) fn verify_signature(secret: &[u8], header: Option<&str>, body: &[u8]) -> bool {
    let Some(signature) = header
        .and_then(|value| value.strip_prefix("sha256="))
        .filter(|hex| hex.len() == 64)
        .and_then(|hex| hex::decode(hex).ok())
    else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

// A person is the only one whose acts are decisions: a bot labelling an issue, this
// app's own comments coming back, and a pull request's conversation are all ignored.
pub(crate) fn read_change(event: &str, body: &[u8]) -> Result<Option<IssueChange>, String> {
    if !matches!(event, "issues" | "issue_comment") {
        return Ok(None);
    }
    let payload: Payload = serde_json::from_slice(body)
        .map_err(|error| format!("the {event} event could not be read: {error}"))?;
    if payload.sender.account_type != "User" || payload.issue.pull_request.is_some() {
        return Ok(None);
    }
    let Some(issue) = IssueAddress::from_url(&payload.issue.html_url) else {
        return Err("the event names an issue without a usable URL".to_owned());
    };
    let actor_id = payload.sender.id.to_string();
    let is_delegation_label = payload
        .label
        .as_ref()
        .is_some_and(|label| label.name.eq_ignore_ascii_case(DELEGATION_LABEL));
    Ok(match (event, payload.action.as_str()) {
        ("issues", "labeled") if is_delegation_label => Some(IssueChange::Delegated {
            issue,
            delegator_id: actor_id,
            delegator_login: payload.sender.login,
        }),
        ("issues", "unlabeled") if is_delegation_label => Some(IssueChange::Withdrawn),
        ("issues", "closed" | "reopened") => Some(IssueChange::StateChanged {
            issue,
            state_id: workflow_state(&payload.issue.rest).id,
            actor_id,
        }),
        ("issue_comment", "created") => {
            let Some(comment) = payload.comment else {
                return Ok(None);
            };
            let written_at_ms = chrono::DateTime::parse_from_rfc3339(&comment.created_at)
                .map(|at| at.timestamp_millis())
                .map_err(|_| "the comment has no readable time".to_owned())?;
            Some(IssueChange::Commented {
                comment: WrittenComment {
                    issue,
                    number: comment.id,
                },
                author_id: actor_id,
                body: comment.body,
                written_at_ms,
            })
        }
        _ => None,
    })
}
