use std::{fmt, sync::RwLock};

use serde_json::{Value, json};

use super::scope_of;
use crate::task::TaskPlanStep;
use crate::tracker::host::automation::{
    DelegatedIssue, IssueDelegation, SessionStanding, TrackerAutomation,
};
use crate::tracker::{
    IssueReference, IssueSnapshot, TrackerError, TrackerFuture, TrackerKind, TrackerStatus,
};

mod agent_sessions;
mod comments;
mod issues;
mod queries;
mod sessions;

pub use agent_sessions::{
    AgentActivityContent, AgentSessionEvent, CommentSite, WebhookError, parse_agent_session_event,
    verify_signature,
};

use self::issues::summary_of;
use self::queries::*;

// Linear refuses a request about something it does not have rather than answering
// with nothing, and that is the one refusal that is not a failure: a question about
// a comment or an issue that is gone is answered, not retried forever.
pub fn is_absence(refusal: &str) -> bool {
    refusal.contains("not found") || refusal.contains("Entity not found")
}

pub struct LinearAutomation {
    token: RwLock<String>,
    workspace_id: String,
    app_user_id: String,
    client: reqwest::Client,
}

impl fmt::Debug for LinearAutomation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The token is deliberately absent, not redacted: a redacted field still
        // invites someone to add a formatter that prints it.
        formatter
            .debug_struct("LinearAutomation")
            .field("workspace_id", &self.workspace_id)
            .field("app_user_id", &self.app_user_id)
            .finish()
    }
}

impl LinearAutomation {
    pub fn new(
        token: impl Into<String>,
        workspace_id: impl Into<String>,
        app_user_id: impl Into<String>,
    ) -> Result<Self, TrackerError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| TrackerError::new("could not build the Linear HTTP client"))?;
        Ok(Self {
            token: RwLock::new(token.into()),
            workspace_id: workspace_id.into(),
            app_user_id: app_user_id.into(),
            client,
        })
    }

    pub async fn verify(&self) -> Result<(), TrackerError> {
        let data = self.graphql(VIEWER_QUERY, json!({})).await?;
        let viewer = data
            .get("viewer")
            .ok_or_else(|| TrackerError::new("Linear returned no viewer for this token"))?;
        if viewer.get("app").and_then(Value::as_bool) != Some(true) {
            return Err(TrackerError::new(
                "the Linear token is not an app user token",
            ));
        }
        let id = string_field(viewer, "id")?;
        if id != self.app_user_id {
            return Err(TrackerError::new(
                "the Linear token belongs to a different app user",
            ));
        }
        let organization = viewer
            .pointer("/organization/id")
            .and_then(Value::as_str)
            .ok_or_else(|| TrackerError::new("Linear returned a viewer without a workspace"))?;
        if organization != self.workspace_id {
            return Err(TrackerError::new(
                "the Linear token belongs to a different workspace",
            ));
        }
        Ok(())
    }

    pub async fn read(&self, reference: &IssueReference) -> Result<IssueSnapshot, TrackerError> {
        let data = self
            .graphql(ISSUE_QUERY, json!({ "id": reference.id }))
            .await?;
        let issue = data
            .get("issue")
            .ok_or_else(|| TrackerError::new("Linear did not return that issue"))?;
        self.ensure_workspace(issue)?;
        let comments = self.comments(&reference.id).await?;
        Ok(IssueSnapshot {
            blocked_by: Vec::new(),
            issue: summary_of(issue)?,
            description: issue
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            comments,
        })
    }

    fn ensure_workspace(&self, issue: &Value) -> Result<(), TrackerError> {
        let organization = issue
            .pointer("/team/organization/id")
            .and_then(Value::as_str)
            .ok_or_else(|| TrackerError::new("the Linear issue did not name its workspace"))?;
        if organization != self.workspace_id {
            return Err(TrackerError::new(
                "this issue belongs to a different Linear workspace than this connection",
            ));
        }
        Ok(())
    }

    pub fn replace_token(&self, token: String) -> Result<(), TrackerError> {
        *self
            .token
            .write()
            .map_err(|_| TrackerError::new("Linear credential lock is unavailable"))? = token;
        Ok(())
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn app_user_id(&self) -> &str {
        &self.app_user_id
    }

    // HTTP success alone does not establish that a GraphQL operation succeeded.
    // A picture goes into Linear's own storage, which is what an issue comment can
    // show. The bytes were vetted when they entered altitude, so nothing is checked
    // again here: this is a courier.

    pub async fn upload_picture(
        &self,
        bytes: &[u8],
        mime: &str,
        name: &str,
    ) -> Result<String, TrackerError> {
        let reservation = self
            .graphql(
                FILE_UPLOAD_MUTATION,
                json!({ "filename": name, "size": bytes.len(), "contentType": mime }),
            )
            .await?;
        let upload = reservation
            .get("fileUpload")
            .and_then(|upload| upload.get("uploadFile"))
            .ok_or_else(|| TrackerError::new("Linear did not describe where to put the picture"))?;
        let url = upload
            .get("uploadUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| TrackerError::new("Linear gave no place to put the picture"))?;
        let asset = upload
            .get("assetUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| TrackerError::new("Linear gave no address for the picture"))?;
        let mut request = self.client.put(url).body(bytes.to_vec());
        if let Some(headers) = upload.get("headers").and_then(Value::as_array) {
            for header in headers {
                if let (Some(key), Some(value)) = (
                    header.get("key").and_then(Value::as_str),
                    header.get("value").and_then(Value::as_str),
                ) {
                    request = request.header(key, value);
                }
            }
        }
        let response = request
            .send()
            .await
            .map_err(|_| TrackerError::new("the picture could not be sent to Linear"))?;
        if !response.status().is_success() {
            return Err(TrackerError::new(format!(
                "Linear refused the picture with HTTP {}",
                response.status()
            )));
        }
        Ok(asset.to_owned())
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value, TrackerError> {
        let token = self
            .token
            .read()
            .map_err(|_| TrackerError::new("Linear credential lock is unavailable"))?
            .clone();
        let response = self
            .client
            .post(LINEAR_GRAPHQL_URL)
            .bearer_auth(token)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|_| TrackerError::new("Linear did not answer"))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|_| TrackerError::new("Linear returned a response that was not JSON"))?;
        if !status.is_success() {
            // A refused request carries the reason in its body, and a status alone
            // sends whoever reads the log to guess at it.
            return Err(TrackerError::new(format!(
                "Linear rejected the request with HTTP {status}: {}",
                describe_refusal(&payload)
            )));
        }
        if let Some(errors) = payload.get("errors") {
            let errors = errors
                .as_array()
                .ok_or_else(|| TrackerError::new("Linear returned invalid GraphQL errors"))?;
            if !errors.is_empty() {
                return Err(TrackerError::new(graphql_error_message(errors)));
            }
        }
        payload
            .get("data")
            .cloned()
            .ok_or_else(|| TrackerError::new("Linear returned no data"))
    }
}

pub(super) fn string_field(value: &Value, field: &str) -> Result<String, TrackerError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TrackerError::new(format!("the Linear response had no {field}")))
}
pub(super) fn describe_refusal(payload: &Value) -> String {
    if let Some(errors) = payload.get("errors").and_then(Value::as_array) {
        let messages: Vec<String> = errors
            .iter()
            .filter_map(|error| error.get("message").and_then(Value::as_str))
            .map(|message| message.chars().take(200).collect())
            .collect();
        if !messages.is_empty() {
            return messages.join("; ");
        }
    }
    let text = payload.to_string();
    let text: String = text.chars().take(200).collect();
    text
}
pub(super) fn graphql_error_message(errors: &[Value]) -> String {
    let detail = errors
        .iter()
        .find_map(|error| error.get("message").and_then(Value::as_str))
        .unwrap_or("Linear rejected the request");
    let detail: String = detail.chars().take(200).collect();
    format!("Linear rejected the request: {detail}")
}

// The remote's report loop speaks to Linear through the same calls it makes of any
// tracker; these say which of Linear's own they are.
impl TrackerAutomation for LinearAutomation {
    fn kind(&self) -> TrackerKind {
        TrackerKind::Linear
    }

    fn app_user_id(&self) -> &str {
        &self.app_user_id
    }

    fn is_absence(&self, refusal: &str) -> bool {
        is_absence(refusal)
    }

    fn issue_reference(&self, issue_id: &str) -> IssueReference {
        IssueReference {
            provider: TrackerKind::Linear,
            workspace_id: self.workspace_id.clone(),
            id: issue_id.to_owned(),
            identifier: String::new(),
            url: String::new(),
        }
    }

    fn upload_picture<'a>(
        &'a self,
        bytes: &'a [u8],
        mime: &'a str,
        name: &'a str,
    ) -> TrackerFuture<'a, String> {
        Box::pin(Self::upload_picture(self, bytes, mime, name))
    }

    fn comment_site<'a>(&'a self, comment_id: &'a str) -> TrackerFuture<'a, CommentSite> {
        Box::pin(Self::comment_site(self, comment_id))
    }

    fn comment_reply<'a>(
        &'a self,
        issue_id: Option<&'a str>,
        document_content_id: Option<&'a str>,
        parent_id: &'a str,
        body: &'a str,
        id: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::comment_reply(
            self,
            issue_id,
            document_content_id,
            parent_id,
            body,
            id,
        ))
    }

    fn react_to_comment<'a>(
        &'a self,
        comment_id: &'a str,
        emoji: &'a str,
        id: Option<&'a str>,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::react_to_comment(self, comment_id, emoji, id))
    }

    fn update_session_summary<'a>(
        &'a self,
        session_id: &'a str,
        summary: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::update_session_summary(self, session_id, summary))
    }

    fn update_session_plan<'a>(
        &'a self,
        session_id: &'a str,
        steps: &'a [TaskPlanStep],
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::update_session_plan(self, session_id, steps))
    }

    fn user_url<'a>(&'a self, user_id: &'a str) -> TrackerFuture<'a, String> {
        Box::pin(Self::user_url(self, user_id))
    }

    fn user_mention<'a>(&'a self, user_id: &'a str) -> TrackerFuture<'a, String> {
        Box::pin(Self::user_mention(self, user_id))
    }

    fn user_name<'a>(&'a self, user_id: &'a str) -> TrackerFuture<'a, String> {
        Box::pin(Self::user_name(self, user_id))
    }

    fn create_session_on_issue<'a>(&'a self, issue_id: &'a str) -> TrackerFuture<'a, String> {
        Box::pin(Self::create_session_on_issue(self, issue_id))
    }

    fn session_standing<'a>(&'a self, session_id: &'a str) -> TrackerFuture<'a, SessionStanding> {
        Box::pin(Self::session_standing(self, session_id))
    }

    fn create_activity<'a>(
        &'a self,
        session_id: &'a str,
        activity_id: &'a str,
        content: AgentActivityContent,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::create_activity(
            self,
            session_id,
            activity_id,
            content,
        ))
    }

    fn create_ephemeral_thought<'a>(
        &'a self,
        session_id: &'a str,
        activity_id: &'a str,
        body: String,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::create_ephemeral_thought(
            self,
            session_id,
            activity_id,
            body,
        ))
    }

    fn delegated_issues(&self) -> TrackerFuture<'_, Vec<DelegatedIssue>> {
        Box::pin(Self::delegated_issues(self))
    }

    fn issue_delegation<'a>(&'a self, issue_id: &'a str) -> TrackerFuture<'a, IssueDelegation> {
        Box::pin(Self::issue_delegation(self, issue_id))
    }

    fn status<'a>(
        &'a self,
        reference: &'a IssueReference,
        status: TrackerStatus,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::status(self, reference, status))
    }

    fn release_issue<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, ()> {
        Box::pin(Self::release_issue(self, reference))
    }

    fn claim_issue<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, ()> {
        Box::pin(Self::claim_issue(self, reference))
    }
}
