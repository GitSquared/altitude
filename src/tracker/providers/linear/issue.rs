
use super::url::workspace_slug_from_url;
use super::*;

use serde_json::{Map, Value, json};

use crate::tracker::{
    IssueComment, IssueReference,
    IssueSummary, TrackerAccount, TrackerError, TrackerKind,
};

pub(super) async fn read_account(session: &Session) -> Result<TrackerAccount, TrackerError> {
    // `whoami` answers the person and the workspace in one call, which is what a
    // login is named by here. `get_user` knows only the person, and being signed in
    // is not the same as knowing where: a name without a workspace is one of two
    // logins that read the same.
    let data = if session.has(WHOAMI) {
        session.call(WHOAMI, Map::new()).await
    } else {
        let parameter = if session.accepts("get_user", "query") {
            "query"
        } else {
            "id"
        };
        let mut arguments = Map::new();
        arguments.insert(parameter.into(), json!("me"));
        session.call("get_user", arguments).await
    }?;
    let user = data.get("user").unwrap_or(&data);
    Ok(TrackerAccount {
        provider: TrackerKind::Linear,
        user_id: string_field(user, "id")?,
        display_name: user
            .get("displayName")
            .or_else(|| user.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| TrackerError::new("Linear returned no name for your account"))?
            .to_owned(),
        workspace: workspace_of(user).or_else(|| workspace_named(&data)),
    })
}

pub(super) fn workspace_named(data: &Value) -> Option<String> {
    let named = data
        .get("workspace")?
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|named| !named.is_empty())?;
    Some(named.to_owned())
}

pub(super) fn workspace_of(user: &Value) -> Option<String> {
    let workspace = user
        .get("organization")
        .or_else(|| user.get("workspace"))
        .or_else(|| user.get("team").and_then(|team| team.get("organization")))?;
    let named = workspace
        .get("urlKey")
        .or_else(|| workspace.get("key"))
        .or_else(|| workspace.get("slug"))
        .or_else(|| workspace.get("name"))
        .and_then(Value::as_str)?
        .trim();
    (!named.is_empty()).then(|| named.to_owned())
}

pub(super) async fn fetch_issue(session: &Session, id: &str) -> Result<Value, TrackerError> {
    let mut arguments = Map::new();
    arguments.insert("id".to_owned(), json!(id));
    match session.call(GET_ISSUE, arguments).await {
        Ok(value) => Ok(issue_object(value)),
        Err(error) if is_not_found(&error) => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

pub(super) fn issue_object(value: Value) -> Value {
    match value {
        Value::Object(mut map) => match map.remove("issue") {
            Some(issue) => issue,
            None => Value::Object(map),
        },
        other => other,
    }
}

pub(super) fn issue_list(value: &Value) -> Result<Vec<Value>, TrackerError> {
    object_list(value, "issues")
}

pub(super) fn object_list(value: &Value, name: &str) -> Result<Vec<Value>, TrackerError> {
    let array = match value {
        Value::Array(items) => items,
        Value::Object(map) => map.get(name).and_then(Value::as_array).ok_or_else(|| {
            TrackerError::new(format!("the Linear {name} list had an unexpected shape"))
        })?,
        _ => {
            return Err(TrackerError::new(format!(
                "the Linear {name} list had an unexpected shape"
            )));
        }
    };
    Ok(array.clone())
}

pub(super) fn issue_url(issue: &Value) -> Result<String, TrackerError> {
    issue
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TrackerError::new("the Linear server returned an issue without a URL"))
}

pub(super) fn issue_description(issue: &Value) -> String {
    issue
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(super) fn summary_of(issue: &Value) -> Result<IssueSummary, TrackerError> {
    // Linear's MCP server answers with the identifier in `id` and the canonical
    // uuid beside it, while webhooks and state changes speak uuid. A reference is
    // compared against those, so the uuid is what `id` has to hold; `identifier` is
    // the name people use.
    let id = issue
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .map(Ok)
        .unwrap_or_else(|| string_field(issue, "id"))?;
    let url = issue_url(issue)?;
    // The identifier is the operator's name for the issue — `ALT-123`. A row of a
    // search does not always carry it, though the URL always spells it, so the
    // URL answers when the field does not.
    let identifier = match string_field(issue, "identifier") {
        Ok(identifier) => identifier,
        Err(missing) => issue_identifier_from_url(&url).ok_or(missing)?,
    };
    let workspace = workspace_slug_from_url(&url)
        .ok_or_else(|| TrackerError::new("the Linear issue URL has no workspace"))?;
    let project = issue
        .get("project")
        .and_then(Value::as_object)
        .and_then(|project| project.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(IssueSummary {
        reference: IssueReference {
            provider: TrackerKind::Linear,
            workspace_id: workspace,
            id,
            identifier,
            url,
        },
        title: string_field(issue, "title")?,
        project,
        scope: scope_of(issue)?,
    })
}

pub(super) fn string_field(issue: &Value, field: &str) -> Result<String, TrackerError> {
    issue
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TrackerError::new(format!("the Linear issue had no {field}")))
}

pub(super) async fn read_comments(
    session: &Session,
    issue_id: &str,
) -> Result<Vec<IssueComment>, TrackerError> {
    let mut comments = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_COMMENT_PAGES {
        let mut arguments = Map::new();
        arguments.insert("issueId".to_owned(), json!(issue_id));
        if session.accepts(LIST_COMMENTS, "limit") {
            arguments.insert("limit".to_owned(), json!(PAGE_LIMIT));
        }
        if let Some(cursor) = &cursor {
            arguments.insert("cursor".to_owned(), json!(cursor));
        }
        let value = session.call(LIST_COMMENTS, arguments).await?;
        let (mut page, next) = comment_page(&value)?;
        comments.append(&mut page);
        match next {
            Some(next) => cursor = Some(next),
            None => {
                comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
                return Ok(comments);
            }
        }
    }
    Err(TrackerError::new(format!(
        "this issue has more than {} comments; read the rest in the Linear app",
        MAX_COMMENT_PAGES * PAGE_LIMIT as usize
    )))
}

pub(super) fn comment_page(
    value: &Value,
) -> Result<(Vec<IssueComment>, Option<String>), TrackerError> {
    let (items, container) = match value {
        Value::Array(items) => (items, None),
        Value::Object(map) => {
            let items = map
                .get("comments")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    TrackerError::new("the Linear comment list had an unexpected shape")
                })?;
            (items, Some(map))
        }
        _ => {
            return Err(TrackerError::new(
                "the Linear comment list had an unexpected shape",
            ));
        }
    };
    let comments = items
        .iter()
        .map(comment_of)
        .collect::<Result<Vec<_>, _>>()?;
    let next = container
        .and_then(|map| map.get("cursor"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let more = container
        .and_then(|map| map.get("hasNextPage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if more && next.is_none() {
        return Err(TrackerError::new(
            "Linear reported more discussion but gave no cursor",
        ));
    }
    Ok((comments, next))
}

pub(super) fn comment_of(node: &Value) -> Result<IssueComment, TrackerError> {
    let author = node
        .get("user")
        .and_then(Value::as_object)
        .and_then(|user| user.get("displayName").or_else(|| user.get("name")))
        .and_then(Value::as_str)
        .or_else(|| node.get("botActor").and_then(Value::as_str))
        .unwrap_or("Unknown")
        .to_owned();
    Ok(IssueComment {
        author,
        body: string_field(node, "body")?,
        created_at: string_field(node, "createdAt")?,
    })
}
