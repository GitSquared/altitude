use serde_json::{Value, json};

use crate::tracker::{AskSurface, IssueReference, TrackerError, TrackerFuture};

use super::agent_sessions::CommentSite;
use super::issues::mutation_succeeded;
use super::queries::*;
use super::string_field;
use super::*;

impl LinearAutomation {
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

    pub async fn comment(&self, comment_id: &str) -> Result<Option<Value>, TrackerError> {
        let data = match self
            .graphql(COMMENT_EXISTS_QUERY, json!({ "id": comment_id }))
            .await
        {
            Ok(data) => data,
            // Asking about a comment this machine published is how a thread that was
            // deleted or never landed is found, and "there is none" is the answer.
            Err(error) if is_absence(&error.to_string()) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(data
            .get("comment")
            .filter(|comment| !comment.is_null())
            .cloned())
    }

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

    pub async fn reopen_comment(&self, comment_id: &str) -> Result<(), TrackerError> {
        let Some(comment) = self.comment(comment_id).await? else {
            return Ok(());
        };
        if comment.get("resolvedAt").is_some_and(Value::is_null) {
            return Ok(());
        }
        let data = self
            .graphql(COMMENT_UNRESOLVE_MUTATION, json!({ "id": comment_id }))
            .await?;
        mutation_succeeded(&data, "commentUnresolve").map(|_| ())
    }

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
}

impl AskSurface for LinearAutomation {
    fn open_ask<'a>(
        &'a self,
        issue: &'a IssueReference,
        marker: &'a str,
        body: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async move { self.comment_on_issue(&issue.id, body, marker).await })
    }

    fn answer_ask<'a>(
        &'a self,
        issue: &'a IssueReference,
        marker: &'a str,
        reply_marker: &'a str,
        body: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async move {
            self.comment_reply(Some(&issue.id), None, marker, body, reply_marker)
                .await
        })
    }

    fn close_ask<'a>(
        &'a self,
        marker: &'a str,
        closed: bool,
        resolving: Option<&'a str>,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(async move {
            if closed {
                self.resolve_comment(marker, resolving).await
            } else {
                self.reopen_comment(marker).await
            }
        })
    }

    fn has_ask<'a>(&'a self, marker: &'a str) -> TrackerFuture<'a, bool> {
        Box::pin(async move { self.comment(marker).await.map(|comment| comment.is_some()) })
    }
}
