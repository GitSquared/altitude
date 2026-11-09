use std::{collections::HashMap, fmt, sync::Mutex, time::Instant};

use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

mod app;
mod comments;
mod sessions;
pub(crate) mod webhook;

pub(crate) use comments::WrittenComment;
pub(crate) use sessions::session_id;

use crate::task::TaskPlanStep;
use crate::tracker::{
    AskSurface, IssueReader, IssueReference, IssueSnapshot, TrackerError, TrackerFuture,
    TrackerKind, TrackerStatus,
    host::automation::{
        AgentActivityContent, CommentSite, DelegatedIssue, IssueDelegation, SessionStanding,
        TrackerAutomation, WorkflowState,
    },
};

use super::api::GitHubApi;
use super::issues::{IssueAddress, snapshot, summary_of, text};
use super::status::{ensure_label, has_label, remove_label, write_status};
use app::AppCredentials;

// The label that hands an issue to altitude, the way Linear's delegate field does.
pub(crate) const DELEGATION_LABEL: &str = "agent:altitude";

pub struct GitHubAutomation {
    api: GitHubApi,
    app: AppCredentials,
    app_slug: String,
    app_user_id: String,
    users: Mutex<HashMap<String, Value>>,
    writing: AsyncMutex<()>,
    status_comments: Mutex<HashMap<String, (u64, String)>>,
    ephemeral_edits: Mutex<HashMap<String, Instant>>,
}

impl fmt::Debug for GitHubAutomation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key is deliberately absent, not redacted: a redacted field still
        // invites someone to add a formatter that prints it.
        formatter
            .debug_struct("GitHubAutomation")
            .field("app", &self.app_slug)
            .field("app_user_id", &self.app_user_id)
            .finish()
    }
}

impl GitHubAutomation {
    // Signing in is asking GitHub which app the key belongs to and which user it
    // writes as: the bot's id is how this app recognises its own comments.
    pub async fn connect(app_id: String, private_key_pem: &[u8]) -> Result<Self, TrackerError> {
        let api = GitHubApi::new()?;
        let app = AppCredentials::new(app_id, private_key_pem)?;
        let described = api.get(&app.jwt()?, "/app").await?;
        let app_slug = text(&described, "slug")?;
        let bot = api
            .get(
                &app.any_installation_token(&api).await?,
                &format!("/users/{app_slug}%5Bbot%5D"),
            )
            .await?;
        let app_user_id = bot
            .get("id")
            .and_then(Value::as_u64)
            .map(|id| id.to_string())
            .ok_or_else(|| TrackerError::new("GitHub named no user for the app"))?;
        Ok(Self {
            api,
            app,
            app_slug,
            app_user_id,
            users: Mutex::new(HashMap::new()),
            writing: AsyncMutex::new(()),
            status_comments: Mutex::new(HashMap::new()),
            ephemeral_edits: Mutex::new(HashMap::new()),
        })
    }

    pub fn app_slug(&self) -> &str {
        &self.app_slug
    }

    pub fn app_user_id(&self) -> &str {
        &self.app_user_id
    }

    async fn token(&self, issue: &IssueAddress) -> Result<String, TrackerError> {
        self.app
            .token_for_repository(&self.api, &issue.repository)
            .await
    }

    pub fn issue_reference(&self, issue_id: &str) -> IssueReference {
        match IssueAddress::parse(issue_id) {
            Some(issue) => issue.reference(format!(
                "https://github.com/{}/issues/{}",
                issue.repository, issue.number
            )),
            None => IssueReference {
                provider: TrackerKind::GitHub,
                workspace_id: String::new(),
                id: issue_id.to_owned(),
                identifier: issue_id.to_owned(),
                url: String::new(),
            },
        }
    }

    pub async fn read(&self, reference: &IssueReference) -> Result<IssueSnapshot, TrackerError> {
        let issue = address(&reference.id)?;
        snapshot(&self.api, &self.token(&issue).await?, &issue).await
    }

    pub async fn upload_picture(
        &self,
        _bytes: &[u8],
        _mime: &str,
        _name: &str,
    ) -> Result<String, TrackerError> {
        Err(TrackerError::new(
            "GitHub has no way for an app to attach a picture to an issue",
        ))
    }

    async fn user(&self, user_id: &str) -> Result<Value, TrackerError> {
        if let Some(user) = self
            .users
            .lock()
            .ok()
            .and_then(|users| users.get(user_id).cloned())
        {
            return Ok(user);
        }
        let user = self
            .api
            .get(
                &self.app.any_installation_token(&self.api).await?,
                &format!("/user/{user_id}"),
            )
            .await?;
        if let Ok(mut users) = self.users.lock() {
            users.insert(user_id.to_owned(), user.clone());
        }
        Ok(user)
    }

    pub async fn user_url(&self, user_id: &str) -> Result<String, TrackerError> {
        text(&self.user(user_id).await?, "html_url")
    }

    pub async fn user_mention(&self, user_id: &str) -> Result<String, TrackerError> {
        Ok(format!("@{}", text(&self.user(user_id).await?, "login")?))
    }

    pub async fn user_name(&self, user_id: &str) -> Result<String, TrackerError> {
        let user = self.user(user_id).await?;
        Ok(user
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or(text(&user, "login")?))
    }

    // Every open issue carrying the label, on every repository the app is installed on,
    // with when the label was last put on it: that is when the delegation was made.
    pub async fn delegated_issues(&self) -> Result<Vec<DelegatedIssue>, TrackerError> {
        let mut delegated = Vec::new();
        for installation in self.app.installations(&self.api).await? {
            let token = self
                .app
                .token_for_installation(&self.api, installation)
                .await?;
            let repositories = self
                .api
                .get(&token, "/installation/repositories?per_page=100")
                .await?;
            let names: Vec<String> = repositories
                .get("repositories")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|repository| {
                    repository
                        .get("full_name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect();
            for repository in names {
                let issues = self
                    .api
                    .pages(
                        &token,
                        &format!(
                            "/repos/{repository}/issues?state=open&per_page=100&labels={}",
                            DELEGATION_LABEL.replace(':', "%3A")
                        ),
                    )
                    .await?;
                for issue in issues
                    .iter()
                    .filter(|issue| issue.get("pull_request").is_none())
                {
                    let Ok(summary) = summary_of(issue) else {
                        continue;
                    };
                    let address = address(&summary.reference.id)?;
                    let delegated_at_ms = self.labelled_at(&token, &address).await;
                    delegated.push(DelegatedIssue {
                        reference: summary.reference,
                        session: None,
                        delegated_at_ms,
                    });
                }
            }
        }
        Ok(delegated)
    }

    async fn labelled_at(&self, token: &str, issue: &IssueAddress) -> Option<i64> {
        let events = self
            .api
            .pages(token, &format!("{}/events?per_page=100", issue.path()))
            .await
            .ok()?;
        events
            .iter()
            .rev()
            .find(|event| {
                event.get("event").and_then(Value::as_str) == Some("labeled")
                    && event.pointer("/label/name").and_then(Value::as_str)
                        == Some(DELEGATION_LABEL)
            })
            .and_then(|event| event.get("created_at").and_then(Value::as_str))
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.timestamp_millis())
    }

    pub async fn issue_delegation(&self, issue_id: &str) -> Result<IssueDelegation, TrackerError> {
        let issue = address(issue_id)?;
        let token = self.token(&issue).await?;
        let fetched = self.api.get(&token, &issue.path()).await?;
        Ok(IssueDelegation {
            delegate_id: has_label(&fetched, DELEGATION_LABEL).then(|| self.app_user_id.clone()),
            state: workflow_state(&fetched),
        })
    }

    pub async fn status(
        &self,
        reference: &IssueReference,
        status: TrackerStatus,
    ) -> Result<(), TrackerError> {
        let issue = address(&reference.id)?;
        write_status(&self.api, &self.token(&issue).await?, &issue, status).await
    }

    // A mention on an issue nobody delegated is the operator asking for the work, and
    // the label is where GitHub records who holds it.
    pub async fn claim_issue(&self, reference: &IssueReference) -> Result<(), TrackerError> {
        let issue = address(&reference.id)?;
        let token = self.token(&issue).await?;
        ensure_label(&self.api, &token, &issue.repository, DELEGATION_LABEL).await?;
        self.api
            .send(
                &token,
                reqwest::Method::POST,
                &format!("{}/labels", issue.path()),
                Some(serde_json::json!({ "labels": [DELEGATION_LABEL] })),
            )
            .await
            .map(|_| ())
    }

    pub async fn release_issue(&self, reference: &IssueReference) -> Result<(), TrackerError> {
        let issue = address(&reference.id)?;
        remove_label(
            &self.api,
            &self.token(&issue).await?,
            &issue,
            DELEGATION_LABEL,
        )
        .await
    }
}

// Open and the two ways of closing are the only states an issue has, and an id that
// names both halves is what a change event can be compared with.
pub(crate) fn workflow_state(issue: &Value) -> WorkflowState {
    let state = issue.get("state").and_then(Value::as_str).unwrap_or("open");
    let reason = issue
        .get("state_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (id, state_type) = match (state, reason) {
        ("closed", "not_planned") => ("closed:not_planned", "canceled"),
        ("closed", _) => ("closed:completed", "completed"),
        _ => ("open", "started"),
    };
    WorkflowState {
        id: id.to_owned(),
        name: id.to_owned(),
        state_type: state_type.to_owned(),
    }
}

fn address(issue_id: &str) -> Result<IssueAddress, TrackerError> {
    IssueAddress::parse(issue_id).ok_or_else(|| TrackerError::new("this is not a GitHub issue"))
}

// The remote's report loop speaks to GitHub through the same calls it makes of any
// tracker; these say which of the app's own they are.
impl TrackerAutomation for GitHubAutomation {
    fn kind(&self) -> TrackerKind {
        TrackerKind::GitHub
    }

    fn app_user_id(&self) -> &str {
        &self.app_user_id
    }

    fn issue_reference(&self, issue_id: &str) -> IssueReference {
        Self::issue_reference(self, issue_id)
    }

    // GitHub answers a request about something it does not have, or no longer has,
    // with 404 or 410.
    fn is_absence(&self, refusal: &str) -> bool {
        refusal.contains("HTTP 404") || refusal.contains("HTTP 410")
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

    // GitHub has no documents, so a reply is always on an issue.
    fn comment_reply<'a>(
        &'a self,
        issue_id: Option<&'a str>,
        _document_content_id: Option<&'a str>,
        parent_id: &'a str,
        body: &'a str,
        id: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::comment_reply(self, issue_id, parent_id, body, id))
    }

    fn react_to_comment<'a>(
        &'a self,
        comment_id: &'a str,
        emoji: &'a str,
        _id: Option<&'a str>,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::react_to_comment(self, comment_id, emoji))
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

    // A passing thought replaces the status line, so it needs no name of its own.
    fn create_ephemeral_thought<'a>(
        &'a self,
        session_id: &'a str,
        _activity_id: &'a str,
        body: String,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::create_ephemeral_thought(self, session_id, body))
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

// An ask is a comment of its own, found again by the marker in its body; closing it
// minimizes it, which is as close to resolved as GitHub's flat feed comes.
impl AskSurface for GitHubAutomation {
    fn open_ask<'a>(
        &'a self,
        issue: &'a IssueReference,
        marker: &'a str,
        body: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(self.comment_on_issue(&issue.id, body, marker))
    }

    fn answer_ask<'a>(
        &'a self,
        issue: &'a IssueReference,
        marker: &'a str,
        reply_marker: &'a str,
        body: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(Self::comment_reply(
            self,
            Some(&issue.id),
            marker,
            body,
            reply_marker,
        ))
    }

    fn close_ask<'a>(
        &'a self,
        issue: &'a IssueReference,
        marker: &'a str,
        closed: bool,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async move {
            if closed {
                self.resolve_comment(&issue.id, marker).await
            } else {
                self.reopen_comment(&issue.id, marker).await
            }
        })
    }

    fn has_ask<'a>(
        &'a self,
        issue: &'a IssueReference,
        marker: &'a str,
    ) -> TrackerFuture<'a, bool> {
        Box::pin(async move {
            self.comment(&issue.id, marker)
                .await
                .map(|comment| comment.is_some())
        })
    }
}

impl IssueReader for GitHubAutomation {
    fn read<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, IssueSnapshot> {
        Box::pin(Self::read(self, reference))
    }
}
