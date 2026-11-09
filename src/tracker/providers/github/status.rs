use reqwest::Method;
use serde_json::{Value, json};
use url::form_urlencoded;

use crate::tracker::{TrackerError, TrackerStatus};

use super::{
    api::{GitHubApi, url_of},
    issues::IssueAddress,
};

// GitHub has open and closed and nothing between, so the two states between are
// labels. An issue has one status, the way a Linear issue has one state: setting one
// takes every other `status:` label off.
const STATUS_PREFIX: &str = "status:";
const IN_PROGRESS_LABEL: &str = "status:in-progress";
const IN_REVIEW_LABEL: &str = "status:in-review";

pub(crate) async fn write_status(
    api: &GitHubApi,
    token: &str,
    address: &IssueAddress,
    status: TrackerStatus,
) -> Result<(), TrackerError> {
    let wanted = match status {
        TrackerStatus::InProgress => Some(IN_PROGRESS_LABEL),
        TrackerStatus::InReview => Some(IN_REVIEW_LABEL),
        TrackerStatus::Done => None,
    };
    let issue = api.get(token, &address.path()).await?;
    for label in status_labels(&issue) {
        if Some(label.as_str()) != wanted {
            remove_label(api, token, address, &label).await?;
        }
    }
    match wanted {
        Some(label) => {
            ensure_label(api, token, &address.repository, label).await?;
            api.send(
                token,
                Method::POST,
                &format!("{}/labels", address.path()),
                Some(json!({ "labels": [label] })),
            )
            .await?;
        }
        None => {
            api.send(
                token,
                Method::PATCH,
                &address.path(),
                Some(json!({ "state": "closed", "state_reason": "completed" })),
            )
            .await?;
        }
    }
    Ok(())
}

pub(crate) fn has_label(issue: &Value, wanted: &str) -> bool {
    label_names(issue).any(|label| label.eq_ignore_ascii_case(wanted))
}

pub(crate) async fn remove_label(
    api: &GitHubApi,
    token: &str,
    address: &IssueAddress,
    label: &str,
) -> Result<(), TrackerError> {
    api.send_tolerating_missing(
        token,
        Method::DELETE,
        &format!("{}/labels/{}", address.path(), encoded(label)),
        None,
    )
    .await
}

// A repository that has never used a label gets it the first time it is needed,
// rather than an issue being labelled with something GitHub invents a colour for.
pub(crate) async fn ensure_label(
    api: &GitHubApi,
    token: &str,
    repository: &str,
    label: &str,
) -> Result<(), TrackerError> {
    let path = format!("/repos/{repository}/labels/{}", encoded(label));
    let existing = api
        .request(token, Method::GET, &url_of(&path), None)
        .await?;
    if existing.status().is_success() {
        return Ok(());
    }
    api.send(
        token,
        Method::POST,
        &format!("/repos/{repository}/labels"),
        Some(json!({ "name": label, "color": "5319e7" })),
    )
    .await
    .map(|_| ())
}

fn status_labels(issue: &Value) -> Vec<String> {
    label_names(issue)
        .filter(|label| label.starts_with(STATUS_PREFIX))
        .map(str::to_owned)
        .collect()
}

fn label_names(issue: &Value) -> impl Iterator<Item = &str> {
    issue
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| label.get("name").and_then(Value::as_str))
}

fn encoded(label: &str) -> String {
    form_urlencoded::byte_serialize(label.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}
