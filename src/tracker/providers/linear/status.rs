use std::cmp::Ordering;

use super::*;

use serde_json::{Map, Value, json};

use crate::tracker::{TrackerError, TrackerStatus};

pub(super) async fn choose_state(
    session: &Session,
    issue: &Value,
    status: TrackerStatus,
) -> Result<String, TrackerError> {
    let team = team_reference(issue).ok_or_else(|| {
        TrackerError::new("the Linear issue did not name a team, so its states are unknown")
    })?;
    let mut arguments = Map::new();
    arguments.insert("team".to_owned(), json!(team));
    let value = session.call(LIST_ISSUE_STATUSES, arguments).await?;
    let states = status_list(&value)?;
    let chosen = match status {
        TrackerStatus::InProgress => states
            .iter()
            .find(|state| state_kind(state) == Some("started") && !is_review(state_name(state))),
        TrackerStatus::InReview => states
            .iter()
            .find(|state| state_kind(state) == Some("started") && is_review(state_name(state))),
        TrackerStatus::Done => states
            .iter()
            .find(|state| state_kind(state) == Some("completed")),
    };
    let state = chosen.ok_or_else(|| TrackerError::new(missing_state_message(status)))?;
    state
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TrackerError::new("a Linear workflow state had no id"))
}

pub(super) fn status_list(value: &Value) -> Result<Vec<Value>, TrackerError> {
    let array = match value {
        Value::Array(items) => items,
        Value::Object(map) => map
            .get("statuses")
            .and_then(Value::as_array)
            .or_else(|| map.get("states").and_then(Value::as_array))
            .ok_or_else(|| TrackerError::new("the Linear status list had an unexpected shape"))?,
        _ => {
            return Err(TrackerError::new(
                "the Linear status list had an unexpected shape",
            ));
        }
    };
    let mut states = array.clone();
    states.sort_by(|left, right| {
        left.get("position")
            .and_then(Value::as_f64)
            .partial_cmp(&right.get("position").and_then(Value::as_f64))
            .unwrap_or(Ordering::Equal)
    });
    Ok(states)
}

pub(super) fn team_reference(issue: &Value) -> Option<&str> {
    issue
        .get("teamId")
        .and_then(Value::as_str)
        .or_else(|| issue.get("team").and_then(Value::as_str))
        .or_else(|| {
            let team = issue.get("team")?.as_object()?;
            team.get("id")
                .or_else(|| team.get("key"))
                .and_then(Value::as_str)
        })
}

pub(super) fn state_name(state: &Value) -> &str {
    state
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

pub(super) fn state_kind(state: &Value) -> Option<&str> {
    state
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| state.get("kind").and_then(Value::as_str))
}

pub(super) fn is_review(name: &str) -> bool {
    let normalized = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    matches!(normalized.as_str(), "in review" | "review")
}

pub(super) fn missing_state_message(status: TrackerStatus) -> String {
    let wanted = match status {
        TrackerStatus::InProgress => "a started workflow state (other than a review state)",
        TrackerStatus::InReview => "a workflow state named \"In Review\" or \"Review\"",
        TrackerStatus::Done => "a completed workflow state",
    };
    format!("this team has no {wanted}; add one in Linear and try again")
}
