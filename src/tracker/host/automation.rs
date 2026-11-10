use serde::{Deserialize, Serialize};

use crate::task::TaskPlanStep;
use crate::tracker::{
    AskSurface, IssueReader, IssueReference, TrackerFuture, TrackerKind, TrackerStatus,
};

// What the remote's report loop asks of a tracker, in Linear's vocabulary because
// Linear's agent sessions are where it comes from. A tracker without sessions
// answers in its own terms: GitHub's session is one comment kept current.

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

pub struct SessionStanding {
    pub can_receive: bool,
    pub never_answered: bool,
}

pub trait TrackerAutomation: AskSurface + IssueReader + Send + Sync + 'static {
    fn kind(&self) -> TrackerKind;

    // The tracker user this app acts as: what it writes comes back as a delivery,
    // and this is how it recognises its own.
    fn app_user_id(&self) -> &str;

    fn issue_reference(&self, issue_id: &str) -> IssueReference;

    // Whether a refusal says the thing asked about is gone. That one is an answer, not
    // a failure, so it is not asked again.
    fn is_absence(&self, refusal: &str) -> bool;

    fn upload_picture<'a>(
        &'a self,
        bytes: &'a [u8],
        mime: &'a str,
        name: &'a str,
    ) -> TrackerFuture<'a, String>;

    fn comment_site<'a>(&'a self, comment_id: &'a str) -> TrackerFuture<'a, CommentSite>;

    fn comment_reply<'a>(
        &'a self,
        issue_id: Option<&'a str>,
        document_content_id: Option<&'a str>,
        parent_id: &'a str,
        body: &'a str,
        id: &'a str,
    ) -> TrackerFuture<'a, ()>;

    fn react_to_comment<'a>(
        &'a self,
        comment_id: &'a str,
        emoji: &'a str,
        id: Option<&'a str>,
    ) -> TrackerFuture<'a, ()>;

    fn update_session_summary<'a>(
        &'a self,
        session_id: &'a str,
        summary: &'a str,
    ) -> TrackerFuture<'a, ()>;

    fn update_session_plan<'a>(
        &'a self,
        session_id: &'a str,
        steps: &'a [TaskPlanStep],
    ) -> TrackerFuture<'a, ()>;

    fn user_url<'a>(&'a self, user_id: &'a str) -> TrackerFuture<'a, String>;

    fn user_mention<'a>(&'a self, user_id: &'a str) -> TrackerFuture<'a, String>;

    fn user_name<'a>(&'a self, user_id: &'a str) -> TrackerFuture<'a, String>;

    fn create_session_on_issue<'a>(&'a self, issue_id: &'a str) -> TrackerFuture<'a, String>;

    fn session_standing<'a>(&'a self, session_id: &'a str) -> TrackerFuture<'a, SessionStanding>;

    fn create_activity<'a>(
        &'a self,
        session_id: &'a str,
        activity_id: &'a str,
        content: AgentActivityContent,
    ) -> TrackerFuture<'a, ()>;

    fn create_ephemeral_thought<'a>(
        &'a self,
        session_id: &'a str,
        activity_id: &'a str,
        body: String,
    ) -> TrackerFuture<'a, ()>;

    fn delegated_issues(&self) -> TrackerFuture<'_, Vec<DelegatedIssue>>;

    fn issue_delegation<'a>(&'a self, issue_id: &'a str) -> TrackerFuture<'a, IssueDelegation>;

    fn status<'a>(
        &'a self,
        reference: &'a IssueReference,
        status: TrackerStatus,
    ) -> TrackerFuture<'a, ()>;

    fn release_issue<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, ()>;

    // Records on the issue that this app holds its work, for work a mention asked for
    // on an issue nobody delegated.
    fn claim_issue<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, ()>;
}
