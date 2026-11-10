use reqwest::Method;
use serde_json::{Value, json};

use crate::tracker::{TrackerError, host::automation::CommentSite};

use super::super::issues::{IssueAddress, author_login, text};
use super::GitHubAutomation;

// Linear takes the id a comment is created with, and altitude names what it publishes
// from the task and the signal so either side can work the id out. GitHub picks its
// own ids, so the name altitude chose rides in the comment as a marker it can be
// found by again: the comment is still the only record of it.
const ID_MARKER: &str = "<!-- altitude:id ";
const MARKER_END: &str = " -->";

const MINIMIZE: &str = "mutation($id: ID!) { minimizeComment(input: { subjectId: $id, classifier: RESOLVED }) { clientMutationId } }";
const UNMINIMIZE: &str =
    "mutation($id: ID!) { unminimizeComment(input: { subjectId: $id }) { clientMutationId } }";
const IS_MINIMIZED: &str =
    "query($id: ID!) { node(id: $id) { ... on IssueComment { isMinimized } } }";

// A comment someone wrote is named by the issue it is on and GitHub's number for it,
// which is everything a later call needs to find it without being told the issue.
pub(crate) struct WrittenComment {
    pub(crate) issue: IssueAddress,
    pub(crate) number: u64,
}

impl WrittenComment {
    pub(crate) fn id(&self) -> String {
        format!("{}/comments/{}", self.issue.id(), self.number)
    }

    pub(crate) fn parse(id: &str) -> Option<Self> {
        let (issue, number) = id.rsplit_once("/comments/")?;
        Some(Self {
            issue: IssueAddress::parse(issue)?,
            number: number.parse().ok()?,
        })
    }

    fn path(&self) -> String {
        format!(
            "/repos/{}/issues/comments/{}",
            self.issue.repository, self.number
        )
    }
}

pub(super) fn marked(body: &str, id: &str) -> String {
    format!("{}\n\n{ID_MARKER}{id}{MARKER_END}", body.trim_end())
}

fn carries_marker(comment: &Value, marker: &str) -> bool {
    comment
        .get("body")
        .and_then(Value::as_str)
        .is_some_and(|body| body.contains(marker))
}

impl GitHubAutomation {
    pub(super) async fn issue_comments(
        &self,
        issue: &IssueAddress,
    ) -> Result<Vec<Value>, TrackerError> {
        let token = self.token(issue).await?;
        self.api
            .pages(&token, &format!("{}/comments?per_page=100", issue.path()))
            .await
    }

    // Only a comment this app wrote can be one it published: anybody can paste a
    // marker into a comment of their own.
    pub(super) async fn own_comment_marked(
        &self,
        issue: &IssueAddress,
        marker: &str,
    ) -> Result<Option<Value>, TrackerError> {
        Ok(self
            .issue_comments(issue)
            .await?
            .into_iter()
            .rev()
            .find(|comment| self.is_own(comment) && carries_marker(comment, marker)))
    }

    pub(super) fn is_own(&self, comment: &Value) -> bool {
        comment
            .pointer("/user/id")
            .and_then(Value::as_u64)
            .map(|id| id.to_string())
            .as_deref()
            == Some(self.app_user_id.as_str())
    }

    async fn published(
        &self,
        issue_id: &str,
        id: &str,
    ) -> Result<Option<(IssueAddress, Value)>, TrackerError> {
        let issue = IssueAddress::parse(issue_id)
            .ok_or_else(|| TrackerError::new("this is not a GitHub issue"))?;
        let found = self
            .own_comment_marked(&issue, &format!("{ID_MARKER}{id}{MARKER_END}"))
            .await?;
        Ok(found.map(|comment| (issue, comment)))
    }

    pub(crate) async fn comment_on_issue(
        &self,
        issue_id: &str,
        body: &str,
        id: &str,
    ) -> Result<(), TrackerError> {
        let body = marked(body, id);
        if let Some((issue, existing)) = self.published(issue_id, id).await? {
            // A re-raised signal is the same comment asking again: it is brought up to
            // date and opened rather than posted twice.
            if existing.get("body").and_then(Value::as_str) != Some(body.as_str()) {
                let token = self.token(&issue).await?;
                let number = existing
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| TrackerError::new("GitHub returned a comment with no id"))?;
                let written = WrittenComment { issue, number };
                self.api
                    .send(
                        &token,
                        Method::PATCH,
                        &written.path(),
                        Some(json!({ "body": body })),
                    )
                    .await?;
            }
            return self.set_minimized(&existing, false).await;
        }
        let issue = IssueAddress::parse(issue_id)
            .ok_or_else(|| TrackerError::new("this is not a GitHub issue"))?;
        let token = self.token(&issue).await?;
        self.api
            .send(
                &token,
                Method::POST,
                &format!("{}/comments", issue.path()),
                Some(json!({ "body": body })),
            )
            .await
            .map(|_| ())
    }

    // GitHub's issue comments are one flat list, so a reply is a comment that says
    // what it answers rather than one nested under it.
    pub(crate) async fn comment_reply(
        &self,
        issue_id: Option<&str>,
        parent_id: &str,
        body: &str,
        id: &str,
    ) -> Result<(), TrackerError> {
        let issue_id = match issue_id {
            Some(issue_id) => issue_id.to_owned(),
            None => WrittenComment::parse(parent_id)
                .map(|parent| parent.issue.id())
                .ok_or_else(|| TrackerError::new("a reply needs the issue it belongs to"))?,
        };
        let issue_id = issue_id.as_str();
        let quoted = match self.published(issue_id, parent_id).await? {
            Some((_, parent)) => parent
                .get("body")
                .and_then(Value::as_str)
                .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
                .map(|line| format!("> {line}\n\n"))
                .unwrap_or_default(),
            None => String::new(),
        };
        self.comment_on_issue(issue_id, &format!("{quoted}{body}"), id)
            .await
    }

    pub(crate) async fn comment(
        &self,
        issue_id: &str,
        id: &str,
    ) -> Result<Option<Value>, TrackerError> {
        Ok(self
            .published(issue_id, id)
            .await?
            .map(|(_, comment)| comment))
    }

    // A resolved thread is a minimized comment: GitHub folds it away and labels it
    // resolved, which is the nearest thing it has.
    pub(crate) async fn resolve_comment(
        &self,
        issue_id: &str,
        id: &str,
    ) -> Result<(), TrackerError> {
        match self.published(issue_id, id).await? {
            Some((_, comment)) => self.set_minimized(&comment, true).await,
            None => Ok(()),
        }
    }

    pub(crate) async fn reopen_comment(
        &self,
        issue_id: &str,
        id: &str,
    ) -> Result<(), TrackerError> {
        match self.published(issue_id, id).await? {
            Some((_, comment)) => self.set_minimized(&comment, false).await,
            None => Ok(()),
        }
    }

    async fn set_minimized(&self, comment: &Value, minimized: bool) -> Result<(), TrackerError> {
        let node = text(comment, "node_id")?;
        let issue = comment
            .get("html_url")
            .and_then(Value::as_str)
            .and_then(IssueAddress::from_url)
            .ok_or_else(|| TrackerError::new("GitHub returned a comment without its issue"))?;
        let token = self.token(&issue).await?;
        let state = self
            .api
            .graphql(&token, IS_MINIMIZED, json!({ "id": node }))
            .await?;
        if state.pointer("/node/isMinimized").and_then(Value::as_bool) == Some(minimized) {
            return Ok(());
        }
        let mutation = if minimized { MINIMIZE } else { UNMINIMIZE };
        self.api
            .graphql(&token, mutation, json!({ "id": node }))
            .await
            .map(|_| ())
    }

    pub(crate) async fn react_to_comment(
        &self,
        comment_id: &str,
        emoji: &str,
    ) -> Result<(), TrackerError> {
        let Some(written) = WrittenComment::parse(comment_id) else {
            return Ok(());
        };
        // GitHub has eight reactions, named rather than drawn.
        let content = match emoji {
            "eyes" | "👀" => "eyes",
            "+1" | "thumbsup" | "👍" => "+1",
            "heart" | "❤️" => "heart",
            "rocket" | "🚀" => "rocket",
            _ => return Ok(()),
        };
        let token = self.token(&written.issue).await?;
        self.api
            .send(
                &token,
                Method::POST,
                &format!("{}/reactions", written.path()),
                Some(json!({ "content": content })),
            )
            .await
            .map(|_| ())
    }

    pub(crate) async fn comment_site(&self, comment_id: &str) -> Result<CommentSite, TrackerError> {
        let written = WrittenComment::parse(comment_id)
            .ok_or_else(|| TrackerError::new("this is not a GitHub comment"))?;
        let token = self.token(&written.issue).await?;
        let comment = self.api.get(&token, &written.path()).await?;
        let author_id = comment
            .pointer("/user/id")
            .and_then(Value::as_u64)
            .map(|id| id.to_string());
        Ok(CommentSite {
            comment_id: comment_id.to_owned(),
            body: comment
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            parent_id: None,
            issue_id: Some(written.issue.id()),
            document_content_id: None,
            document_id: None,
            root_comment_id: comment_id.to_owned(),
            root_author_id: author_id.clone(),
            document_creator_id: None,
            author_name: Some(author_login(&comment)),
            // The status comment is this app's own transcript, the way Linear's
            // generated session root is.
            is_artificial_session_root: self.is_own(&comment)
                && carries_marker(&comment, super::sessions::SESSION_MARKER),
            authors_truncated: false,
            human_authors: author_id.into_iter().collect(),
        })
    }
}
