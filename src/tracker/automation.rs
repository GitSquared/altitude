//! Linear's native agent API: an app-token GraphQL transport, the signed
//! AgentSessionEvent webhook, and the comment/reaction calls the coordinator
//! needs to answer a human in a shared issue.
//!
//! The MCP adapter in [`super::Linear`] serves the interactive client, where a
//! person signs in. A remote coordinator has no person and no browser: it acts
//! as the OAuth app itself, with an app-actor token the infrastructure owns, and
//! receives its work through Linear's signed webhooks. This module is that path,
//! kept concrete until a second tracker or a second caller shows the shared
//! shape.
//!
//! The token is a credential. It lives in [`LinearAutomation`], is never logged,
//! never written to a task capsule, and never handed to a delegated agent.

use std::{
    error::Error,
    fmt,
    sync::RwLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;

use super::{
    IssueComment, IssueReader, IssueReference, IssueSnapshot, IssueSummary, TrackerError,
    TrackerKind, TrackerStatus, linear,
};
use crate::task::{TaskPlanStatus, TaskPlanStep};

const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const WEBHOOK_MAX_AGE: Duration = Duration::from_secs(300);
const AGENT_SESSION_EVENT: &str = "AgentSessionEvent";
const VIEWER_QUERY: &str = "query Viewer { viewer { id app organization { id } } }";

const ISSUE_QUERY: &str = "query Issue($id: String!) { issue(id: $id) { \
    id identifier title description url project { name } \
    team { id key name organization { id } } } }";

const ISSUE_COMMENTS_QUERY: &str = "query IssueComments($id: String!, $after: String) { \
    issue(id: $id) { comments(first: 100, after: $after, orderBy: createdAt) { \
    nodes { id body createdAt user { displayName name } } pageInfo { hasNextPage endCursor } } } }";

const ISSUE_DELEGATION_QUERY: &str = "query IssueDelegation($id: String!) { \
    issue(id: $id) { id delegate { id } state { id name type } \
    team { id organization { id } } } }";

const ISSUE_STATES_QUERY: &str = "query IssueStates($id: String!) { \
    issue(id: $id) { id team { id organization { id } \
    states(first: 250) { nodes { id name type position } } } } }";

const CREATE_ACTIVITY_MUTATION: &str = "mutation CreateAgentActivity($input: AgentActivityCreateInput!) { \
    agentActivityCreate(input: $input) { success agentActivity { id } } }";

const CREATE_SESSION_ON_ISSUE_MUTATION: &str = "mutation CreateAgentSessionOnIssue($input: AgentSessionCreateOnIssueInput!) { \
    agentSessionCreateOnIssue(input: $input) { success agentSession { id } } }";

const UPDATE_STATE_MUTATION: &str = "mutation UpdateIssueState($id: String!, $input: IssueUpdateInput!) { \
    issueUpdate(id: $id, input: $input) { success } }";

const COMMENT_SITE_QUERY: &str = "query CommentSite($id: String!) { comment(id: $id) { \
    id body parentId issueId documentContentId isArtificialAgentSessionRoot \
    user { id name } \
    parent { id parentId issueId documentContentId user { id } } \
    issue { id team { id } comments(first: 100) { pageInfo { hasNextPage } \
    nodes { user { id } } } } \
    documentContent { id document { id creator { id } issue { id } \
    comments(first: 100) { pageInfo { hasNextPage } nodes { user { id } } } } } } }";

const CREATE_COMMENT_MUTATION: &str = "mutation CreateComment($input: CommentCreateInput!) { \
    commentCreate(input: $input) { success comment { id } } }";

const COMMENT_EXISTS_QUERY: &str = "query CommentExists($id: String!) { comment(id: $id) { \
    id parentId body resolvedAt } }";

const COMMENT_UPDATE_MUTATION: &str = "mutation UpdateComment($id: String!, $input: CommentUpdateInput!) { \
    commentUpdate(id: $id, input: $input) { success } }";

const COMMENT_UNRESOLVE_MUTATION: &str = "mutation UnresolveComment($id: String!) { \
    commentUnresolve(id: $id) { success } }";

const COMMENT_RESOLVE_MUTATION: &str = "mutation ResolveComment($id: String!, $resolvingCommentId: String) { \
    commentResolve(id: $id, resolvingCommentId: $resolvingCommentId) { success } }";

const CREATE_REACTION_MUTATION: &str = "mutation CreateReaction($input: ReactionCreateInput!) { \
    reactionCreate(input: $input) { success reaction { id } } }";

const USER_URL_QUERY: &str = "query UserUrl($id: String!) { user(id: $id) { id name url } }";

const UPDATE_SESSION_SUMMARY_MUTATION: &str = "mutation UpdateSession($id: String!, $input: AgentSessionUpdateInput!) { \
    agentSessionUpdate(id: $id, input: $input) { success } }";

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

    /// Confirm the token authenticates as the expected app user in the expected
    /// workspace.
    ///
    /// The viewer of an app-actor token is the app user, so `viewer.app` must be
    /// true: a personal token would otherwise read and write as that person.
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
            issue: summary_of(issue)?,
            description: issue
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            comments,
        })
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

    /// The thread, document, and authors around one comment.
    ///
    /// Admission needs facts, not a cached copy of the issue. Every call is a
    /// point-in-time read of the discussion as Linear has it now.
    pub async fn comment_site(&self, comment_id: &str) -> Result<CommentSite, TrackerError> {
        let data = self
            .graphql(COMMENT_SITE_QUERY, json!({ "id": comment_id }))
            .await?;
        let comment = data
            .get("comment")
            .filter(|comment| !comment.is_null())
            .ok_or_else(|| TrackerError::new("Linear did not return that comment"))?;
        let parent = comment.get("parent").filter(|parent| !parent.is_null());
        let root = parent.unwrap_or(comment);
        let document = comment.pointer("/documentContent/document");
        let document_issue_id = document
            .and_then(|document| document.pointer("/issue/id"))
            .and_then(Value::as_str);
        let issue_id = comment
            .get("issueId")
            .and_then(Value::as_str)
            .or_else(|| {
                parent
                    .and_then(|parent| parent.get("issueId"))
                    .and_then(Value::as_str)
            })
            .or(document_issue_id)
            .map(str::to_owned);
        let mut authors = Vec::new();
        let mut push_author = |id: Option<&str>| {
            if let Some(id) = id.filter(|id| !id.is_empty())
                && !authors.iter().any(|known| known == id)
            {
                authors.push(id.to_owned());
            }
        };
        push_author(comment.pointer("/user/id").and_then(Value::as_str));
        push_author(
            parent
                .and_then(|parent| parent.pointer("/user/id"))
                .and_then(Value::as_str),
        );
        for connection in [
            comment.pointer("/issue/comments/nodes"),
            comment.pointer("/documentContent/document/comments/nodes"),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(nodes) = connection.as_array() {
                for node in nodes {
                    push_author(node.pointer("/user/id").and_then(Value::as_str));
                }
            }
        }
        Ok(CommentSite {
            comment_id: string_field(comment, "id")?,
            body: comment
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            parent_id: comment
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            issue_id,
            document_content_id: comment
                .get("documentContentId")
                .and_then(Value::as_str)
                .or_else(|| {
                    parent
                        .and_then(|parent| parent.get("documentContentId"))
                        .and_then(Value::as_str)
                })
                .map(str::to_owned),
            document_id: document
                .and_then(|document| document.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            root_comment_id: string_field(root, "id")?,
            root_author_id: root
                .pointer("/user/id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            document_creator_id: document
                .and_then(|document| document.pointer("/creator/id"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            author_name: comment
                .pointer("/user/name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            is_artificial_session_root: comment
                .get("isArtificialAgentSessionRoot")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            authors_truncated: comment
                .pointer("/issue/comments/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || comment
                    .pointer("/documentContent/document/comments/pageInfo/hasNextPage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            human_authors: authors,
        })
    }

    /// Reply in an issue or document comment thread.
    ///
    /// `parentId` alone is not enough: Linear requires exactly one parent entity
    /// id beside it, so the caller passes the issue or document content the
    /// thread belongs to.
    pub async fn comment_reply(
        &self,
        issue_id: Option<&str>,
        document_content_id: Option<&str>,
        parent_id: &str,
        body: &str,
        id: &str,
    ) -> Result<(), TrackerError> {
        let mut input = json!({ "parentId": parent_id, "body": body, "id": id });
        if let Some(document_content_id) = document_content_id {
            input["documentContentId"] = json!(document_content_id);
        } else if let Some(issue_id) = issue_id {
            input["issueId"] = json!(issue_id);
        } else {
            return Err(TrackerError::new(
                "a comment reply needs the issue or document it belongs to",
            ));
        }
        let outcome = self
            .graphql(CREATE_COMMENT_MUTATION, json!({ "input": input }))
            .await
            .and_then(|data| {
                mutation_succeeded(&data, "commentCreate")?;
                Ok(())
            });
        if let Err(error) = outcome {
            // A response can be lost after creation. Prove the comment with our
            // stable id arrived before creating another one on retry.
            if let Ok(existing) = self
                .graphql(COMMENT_EXISTS_QUERY, json!({ "id": id }))
                .await
                && existing.pointer("/comment/id").and_then(Value::as_str) == Some(id)
                && existing
                    .pointer("/comment/parentId")
                    .and_then(Value::as_str)
                    == Some(parent_id)
            {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }

    /// Post a top-level issue comment under a stable id.
    ///
    /// A signal is re-raised into the same thread, so an existing comment is
    /// updated and reopened rather than duplicated.
    pub async fn comment_on_issue(
        &self,
        issue_id: &str,
        body: &str,
        id: &str,
    ) -> Result<(), TrackerError> {
        let input = json!({ "issueId": issue_id, "body": body, "id": id });
        let outcome = self
            .graphql(CREATE_COMMENT_MUTATION, json!({ "input": input }))
            .await
            .and_then(|data| {
                mutation_succeeded(&data, "commentCreate")?;
                Ok(())
            });
        if let Ok(()) = outcome {
            return Ok(());
        }
        // The comment may already exist from a lost response or a re-raised
        // signal. Update it and make sure it is open before treating it as sent.
        let existing = self
            .graphql(COMMENT_EXISTS_QUERY, json!({ "id": id }))
            .await?;
        if existing.pointer("/comment/id").and_then(Value::as_str) != Some(id)
            || existing
                .pointer("/comment/parentId")
                .and_then(Value::as_str)
                .is_some()
        {
            return outcome;
        }
        if existing.pointer("/comment/body").and_then(Value::as_str) != Some(body) {
            let data = self
                .graphql(
                    COMMENT_UPDATE_MUTATION,
                    json!({ "id": id, "input": { "body": body } }),
                )
                .await?;
            mutation_succeeded(&data, "commentUpdate")?;
        }
        if existing
            .pointer("/comment/resolvedAt")
            .is_some_and(|resolved| !resolved.is_null())
        {
            let data = self
                .graphql(COMMENT_UNRESOLVE_MUTATION, json!({ "id": id }))
                .await?;
            mutation_succeeded(&data, "commentUnresolve")?;
        }
        Ok(())
    }

    /// Resolve a comment thread, optionally naming the reply that resolved it.
    pub async fn resolve_comment(
        &self,
        comment_id: &str,
        resolving_comment_id: Option<&str>,
    ) -> Result<(), TrackerError> {
        let data = self
            .graphql(
                COMMENT_RESOLVE_MUTATION,
                json!({
                    "id": comment_id,
                    "resolvingCommentId": resolving_comment_id,
                }),
            )
            .await?;
        mutation_succeeded(&data, "commentResolve").map(|_| ())
    }

    /// Add one reaction to a comment, optionally under a stable id so a retry is
    /// the same reaction rather than a second one.
    pub async fn react_to_comment(
        &self,
        comment_id: &str,
        emoji: &str,
        id: Option<&str>,
    ) -> Result<(), TrackerError> {
        let mut input = json!({ "commentId": comment_id, "emoji": emoji });
        if let Some(id) = id {
            input["id"] = json!(id);
        }
        let data = self
            .graphql(CREATE_REACTION_MUTATION, json!({ "input": input }))
            .await?;
        mutation_succeeded(&data, "reactionCreate").map(|_| ())
    }

    /// Set the session title. The model row in Linear's details is internal;
    /// the title is the public place to name what is running.
    pub async fn update_session_summary(
        &self,
        session_id: &str,
        summary: &str,
    ) -> Result<(), TrackerError> {
        let data = self
            .graphql(
                UPDATE_SESSION_SUMMARY_MUTATION,
                json!({ "id": session_id, "input": { "summary": summary } }),
            )
            .await?;
        mutation_succeeded(&data, "agentSessionUpdate").map(|_| ())
    }

    /// Mirror the agent's plan onto its Linear Agent Session.
    pub async fn update_session_plan(
        &self,
        session_id: &str,
        steps: &[TaskPlanStep],
    ) -> Result<(), TrackerError> {
        let plan = steps
            .iter()
            .map(|step| {
                json!({
                    "content": step.content,
                    "status": match step.status {
                        TaskPlanStatus::Pending => "pending",
                        TaskPlanStatus::InProgress => "inProgress",
                        TaskPlanStatus::Completed => "completed",
                        TaskPlanStatus::Canceled => "canceled",
                    },
                })
            })
            .collect::<Vec<_>>();
        let data = self
            .graphql(
                UPDATE_SESSION_SUMMARY_MUTATION,
                json!({ "id": session_id, "input": { "plan": plan } }),
            )
            .await?;
        mutation_succeeded(&data, "agentSessionUpdate").map(|_| ())
    }

    /// The profile URL Linear uses for a person.
    ///
    /// A bare profile URL in Markdown renders as a mention, which is how the
    /// redirect names the operator without guessing a workspace slug.
    pub async fn user_url(&self, user_id: &str) -> Result<String, TrackerError> {
        let data = self
            .graphql(USER_URL_QUERY, json!({ "id": user_id }))
            .await?;
        data.pointer("/user/url")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| TrackerError::new("Linear returned no profile URL for that user"))
    }

    /// Sponsor an agent session on an issue through the public Agent API.
    ///
    /// This is how a task that began on a client is reported from the remote
    /// that runs it: the issue need not be delegated to this app, and the
    /// human assignee is left where the operator put them. Linear has no way to
    /// make this idempotent, so a response lost after the session was created
    /// leaves a second session behind; the caller records a route before it
    /// retries so it does not ask twice.
    pub async fn create_session_on_issue(&self, issue_id: &str) -> Result<String, TrackerError> {
        let data = self
            .graphql(
                CREATE_SESSION_ON_ISSUE_MUTATION,
                json!({ "input": { "issueId": issue_id } }),
            )
            .await?;
        let payload = mutation_succeeded(&data, "agentSessionCreateOnIssue")?;
        payload
            .pointer("/agentSession/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| TrackerError::new("Linear created the agent session but returned no id"))
    }

    /// Create one durable agent activity under `activity_id`.
    ///
    /// The id is supplied by the caller so a retry after a lost response is the
    /// same activity rather than a second one.
    pub async fn create_activity(
        &self,
        agent_session_id: &str,
        activity_id: &str,
        content: AgentActivityContent,
    ) -> Result<(), TrackerError> {
        let input = json!({
            "agentSessionId": agent_session_id,
            "id": activity_id,
            "content": content,
        });
        let outcome = self
            .graphql(CREATE_ACTIVITY_MUTATION, json!({ "input": input }))
            .await
            .and_then(|data| mutation_succeeded(&data, "agentActivityCreate").map(|_| ()));
        if let Err(error) = outcome {
            // A response can be lost after creation; prove the immutable activity arrived before retrying.
            let existing = self.graphql(
                "query Activity($id: String!) { agentActivity(id: $id) { id agentSession { id } user { id } } }",
                json!({"id": activity_id}),
            ).await;
            if existing.as_ref().is_ok_and(|data| {
                data.pointer("/agentActivity/id").and_then(Value::as_str) == Some(activity_id)
                    && data
                        .pointer("/agentActivity/agentSession/id")
                        .and_then(Value::as_str)
                        == Some(agent_session_id)
                    && data
                        .pointer("/agentActivity/user/id")
                        .and_then(Value::as_str)
                        == Some(self.app_user_id.as_str())
            }) {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }

    async fn comments(&self, issue_id: &str) -> Result<Vec<IssueComment>, TrackerError> {
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
            "this issue has more discussion than Altitude can read in one pass",
        ))
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
            return Err(TrackerError::new(format!(
                "Linear rejected the request with HTTP {status}"
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

#[derive(Clone, Debug, PartialEq, Eq)]
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

/// The facts around one comment that admission is allowed to use.
///
/// The body is included because a wake hands the operator's words to the task;
/// the coordinator never logs it.
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
    /// A busy thread may have more authors than one page returned. Truncated is
    /// treated as shared because an unknown participant must not cause a wake.
    pub authors_truncated: bool,
    /// Every human who has spoken in this thread or document, from Linear.
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

    /// The human who delegated the work, from the session's `creatorId`.
    ///
    /// The issue's assignee is never a fallback: an automation- or agent-started
    /// session has no responsible human, and inventing one would attribute the
    /// work to whoever happens to be assigned.
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

/// The session a webhook event belongs to, as far as the coordinator reads it.
///
/// Only the fields below are parsed. Linear's published schema also marks the
/// session's `type` as required, and its own deliveries omit it: reading a field
/// the coordinator does not use is how a valid delivery becomes unreadable.
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
    /// The payload's timestamp was outside the freshness window.
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

/// Verify Linear's `linear-signature` header against the exact request body.
///
/// `body` must be the raw bytes Linear signed, before any JSON parsing, and the
/// caller must bound its length before calling. Comparison is constant-time
/// through [`Mac::verify_slice`].
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

/// Parse a verified body into the typed event.
///
/// The envelope's `type` is read first so a webhook for another resource is
/// rejected as unsupported rather than reported as malformed. Freshness and
/// identity are checked by [`AgentSessionEvent::validate`]. The caller bounds
/// the body before calling.
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

fn summary_of(issue: &Value) -> Result<IssueSummary, TrackerError> {
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
        scope: linear::scope_of(issue)?,
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

fn mutation_succeeded<'a>(data: &'a Value, field: &str) -> Result<&'a Value, TrackerError> {
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

fn graphql_error_message(errors: &[Value]) -> String {
    let detail = errors
        .iter()
        .find_map(|error| error.get("message").and_then(Value::as_str))
        .unwrap_or("Linear rejected the request");
    let detail: String = detail.chars().take(200).collect();
    format!("Linear rejected the request: {detail}")
}

fn comment_author(node: &Value) -> String {
    node.pointer("/user/displayName")
        .or_else(|| node.pointer("/user/name"))
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_owned()
}

fn string_field(value: &Value, field: &str) -> Result<String, TrackerError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TrackerError::new(format!("the Linear response had no {field}")))
}

impl IssueReader for LinearAutomation {
    fn read<'a>(
        &'a self,
        reference: &'a IssueReference,
    ) -> super::TrackerFuture<'a, IssueSnapshot> {
        Box::pin(LinearAutomation::read(self, reference))
    }
}
