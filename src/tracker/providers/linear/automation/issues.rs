use serde_json::{Value, json};

use super::scope_of;
use crate::tracker::{
    IssueComment, IssueReader, IssueReference, IssueSnapshot, IssueSummary, TrackerError,
    TrackerKind, TrackerStatus,
};

use super::agent_sessions::{DelegatedIssue, IssueDelegation, WorkflowState};
use super::queries::*;
use super::string_field;
use super::*;
use crate::tracker::TrackerFuture;

impl LinearAutomation {
    pub async fn delegated_issues(&self) -> Result<Vec<DelegatedIssue>, TrackerError> {
        let data = self
            .graphql(DELEGATED_ISSUES_QUERY, json!({ "app": self.app_user_id }))
            .await?;
        let nodes = data
            .get("issues")
            .and_then(|issues| issues.get("nodes"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut issues = Vec::new();
        for node in nodes {
            let Ok(issue) = summary_of(&node) else {
                continue;
            };
            // The newest session, not the first one the API happens to list: a
            // delegation makes a new session each time, and answering in an old one
            // is answering in a thread the operator has moved on from. By creation
            // and not by the last word in it: a session nobody has written into yet
            // is exactly what a delegation that has not been picked up looks like.
            let newest = node
                .pointer("/agentSessions/nodes")
                .and_then(Value::as_array)
                .and_then(|sessions| {
                    sessions.iter().max_by_key(|session| {
                        session
                            .get("createdAt")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    })
                });
            let session = newest
                .and_then(|session| session.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let delegated_at_ms = newest
                .and_then(|session| session.get("createdAt"))
                .and_then(Value::as_str)
                .and_then(|created| {
                    chrono::DateTime::parse_from_rfc3339(created)
                        .ok()
                        .map(|at| at.timestamp_millis())
                });
            issues.push(DelegatedIssue {
                reference: issue.reference,
                session,
                delegated_at_ms,
            });
        }
        Ok(issues)
    }

    pub async fn issue_delegation(&self, issue_id: &str) -> Result<IssueDelegation, TrackerError> {
        let data = self
            .graphql(ISSUE_DELEGATION_QUERY, json!({ "id": issue_id }))
            .await?;
        let issue = data
            .get("issue")
            .ok_or_else(|| TrackerError::new("Linear did not return that issue"))?;
        self.ensure_workspace(issue)?;
        let state = workflow_state(
            issue
                .get("state")
                .ok_or_else(|| TrackerError::new("the Linear issue had no state"))?,
        )?;
        let delegate_id = issue
            .pointer("/delegate/id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(IssueDelegation { delegate_id, state })
    }

    pub async fn status(
        &self,
        reference: &IssueReference,
        status: TrackerStatus,
    ) -> Result<(), TrackerError> {
        let data = self
            .graphql(ISSUE_STATES_QUERY, json!({ "id": reference.id }))
            .await?;
        let issue = data
            .get("issue")
            .ok_or_else(|| TrackerError::new("Linear did not return that issue"))?;
        self.ensure_workspace(issue)?;
        let states = team_states(issue)?;
        let chosen = choose_state(&states, status)
            .ok_or_else(|| TrackerError::new(missing_state_message(status)))?;
        let data = self
            .graphql(
                UPDATE_STATE_MUTATION,
                json!({ "id": reference.id, "input": { "stateId": chosen.id } }),
            )
            .await?;
        mutation_succeeded(&data, "issueUpdate")?;
        Ok(())
    }

    pub async fn release_issue(&self, reference: &IssueReference) -> Result<(), TrackerError> {
        let data = self
            .graphql(
                UPDATE_STATE_MUTATION,
                json!({ "id": reference.id, "input": { "delegateId": null } }),
            )
            .await?;
        mutation_succeeded(&data, "issueUpdate")?;
        Ok(())
    }

    pub(super) async fn comments(&self, issue_id: &str) -> Result<Vec<IssueComment>, TrackerError> {
        let mut comments = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..100 {
            let variables = match &after {
                Some(cursor) => json!({ "id": issue_id, "after": cursor }),
                None => json!({ "id": issue_id }),
            };
            let data = self.graphql(ISSUE_COMMENTS_QUERY, variables).await?;
            let connection = data
                .pointer("/issue/comments")
                .ok_or_else(|| TrackerError::new("Linear returned no discussion for that issue"))?;
            let nodes = connection
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    TrackerError::new("the Linear discussion had an unexpected shape")
                })?;
            for node in nodes {
                comments.push(IssueComment {
                    author: comment_author(node),
                    body: string_field(node, "body")?,
                    created_at: string_field(node, "createdAt")?,
                });
            }
            let has_next = connection
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !has_next {
                return Ok(comments);
            }
            let cursor = connection
                .pointer("/pageInfo/endCursor")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    TrackerError::new("Linear reported more discussion but gave no cursor")
                })?
                .to_owned();
            if Some(cursor.clone()) == after {
                return Err(TrackerError::new(
                    "Linear repeated a discussion cursor; refusing to loop",
                ));
            }
            after = Some(cursor);
        }
        Err(TrackerError::new(
            "this issue has more discussion than altitude can read in one pass",
        ))
    }
}

pub(super) fn summary_of(issue: &Value) -> Result<IssueSummary, TrackerError> {
    let url = string_field(issue, "url")?;
    let workspace = issue
        .get("url")
        .and_then(Value::as_str)
        .and_then(|value| url::Url::parse(value).ok())
        .and_then(|url| {
            url.path_segments()?
                .next()
                .filter(|segment| !segment.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| TrackerError::new("the Linear issue URL did not name its workspace"))?;
    Ok(IssueSummary {
        reference: IssueReference {
            provider: TrackerKind::Linear,
            workspace_id: workspace,
            id: string_field(issue, "id")?,
            identifier: string_field(issue, "identifier")?,
            url,
        },
        title: string_field(issue, "title")?,
        project: issue
            .pointer("/project/name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        scope: scope_of(issue)?,
    })
}

fn workflow_state(value: &Value) -> Result<WorkflowState, TrackerError> {
    serde_json::from_value(value.clone())
        .map_err(|_| TrackerError::new("a Linear workflow state had an unexpected shape"))
}

fn team_states(issue: &Value) -> Result<Vec<WorkflowState>, TrackerError> {
    let nodes = issue
        .pointer("/team/states/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| TrackerError::new("the Linear issue did not name its team's states"))?;
    nodes.iter().map(workflow_state).collect()
}

fn choose_state(states: &[WorkflowState], status: TrackerStatus) -> Option<&WorkflowState> {
    match status {
        TrackerStatus::InProgress => states
            .iter()
            .find(|state| state.state_type == "started" && !is_review(&state.name)),
        TrackerStatus::InReview => states
            .iter()
            .find(|state| state.state_type == "started" && is_review(&state.name)),
        TrackerStatus::Done => states.iter().find(|state| state.state_type == "completed"),
    }
}

fn is_review(name: &str) -> bool {
    let normalized = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    matches!(normalized.as_str(), "in review" | "review")
}

fn missing_state_message(status: TrackerStatus) -> String {
    let wanted = match status {
        TrackerStatus::InProgress => "a started workflow state (other than a review state)",
        TrackerStatus::InReview => "a workflow state named \"In Review\" or \"Review\"",
        TrackerStatus::Done => "a completed workflow state",
    };
    format!("this team has no {wanted}; add one in Linear and try again")
}

pub(super) fn mutation_succeeded<'a>(
    data: &'a Value,
    field: &str,
) -> Result<&'a Value, TrackerError> {
    let payload = data
        .get(field)
        .ok_or_else(|| TrackerError::new(format!("Linear returned no {field} result")))?;
    if payload.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(TrackerError::new(format!(
            "Linear did not apply the {field} change"
        )));
    }
    Ok(payload)
}

fn comment_author(node: &Value) -> String {
    node.pointer("/user/displayName")
        .or_else(|| node.pointer("/user/name"))
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_owned()
}

impl IssueReader for LinearAutomation {
    fn read<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, IssueSnapshot> {
        Box::pin(LinearAutomation::read(self, reference))
    }
}
