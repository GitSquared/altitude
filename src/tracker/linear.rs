//! The Linear tracker adapter.
//!
//! [`Linear`] speaks to Linear's official MCP server through the `mcp-remote`
//! stdio bridge, driven by the official rmcp client. OAuth and token storage
//! belong to the bridge and live in a host-only profile directory; this adapter
//! never sees a credential and the profile is never mounted into a capsule.

use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use crate::isolation::private_directory;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool},
    service::RunningService,
    transport::TokioChildProcess,
};
use serde_json::{Map, Value, json};
use tokio::{process::Command, sync::Mutex, time::timeout};

use super::{
    IssueComment, IssueReference, IssueSnapshot, IssueSummary, TrackerError, TrackerFuture,
    TrackerKind, TrackerProvider, TrackerStatus,
};

const LINEAR_MCP_URL: &str = "https://mcp.linear.app/mcp";
const BRIDGE_PACKAGE: &str = "mcp-remote@0.14.3";
const BRIDGE_STORE: &str = "mcp-remote-v1";

const LIST_ISSUES: &str = "list_issues";
const GET_ISSUE: &str = "get_issue";
const LIST_COMMENTS: &str = "list_comments";
const LIST_ISSUE_STATUSES: &str = "list_issue_statuses";
const SAVE_ISSUE: &str = "save_issue";
const SAVE_COMMENT: &str = "save_comment";

const REQUIRED_TOOLS: [&str; 6] = [
    LIST_ISSUES,
    GET_ISSUE,
    LIST_COMMENTS,
    LIST_ISSUE_STATUSES,
    SAVE_ISSUE,
    SAVE_COMMENT,
];

const SEARCH_LIMIT: u64 = 25;
const PAGE_LIMIT: u64 = 100;
const MAX_COMMENT_PAGES: usize = 20;
/// Bounds sign-in and startup, leaving room for the bridge's own 120s OAuth
/// callback window.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(180);
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

struct Session {
    client: RunningService<RoleClient, ()>,
    tools: HashMap<String, Tool>,
}

impl Session {
    async fn start(profile: &Path) -> Result<Self, TrackerError> {
        // stderr is discarded rather than inherited: the bridge's own logs can
        // carry sign-in URLs and tokens, and none of that belongs in Altitude's
        // output.
        let (transport, _) = TokioChildProcess::builder(bridge_command(profile))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| TrackerError::new("could not start the mcp-remote bridge"))?;
        let mut client = ().serve(transport).await.map_err(|_| {
            TrackerError::new("could not connect to Linear; try Connect in Settings → Trackers")
        })?;

        let tools = match client.list_all_tools().await {
            Ok(tools) => tools,
            Err(_) => {
                let _ = client.close_with_timeout(CLEANUP_TIMEOUT).await;
                return Err(TrackerError::new("could not read the Linear tool list"));
            }
        };
        let tools: HashMap<String, Tool> = tools
            .into_iter()
            .map(|tool| (tool.name.to_string(), tool))
            .collect();
        let missing: Vec<&str> = REQUIRED_TOOLS
            .iter()
            .copied()
            .filter(|name| !tools.contains_key(*name))
            .collect();
        if !missing.is_empty() {
            let _ = client.close_with_timeout(CLEANUP_TIMEOUT).await;
            return Err(TrackerError::new(format!(
                "the Linear MCP server is missing required tools: {}",
                missing.join(", ")
            )));
        }
        Ok(Self { client, tools })
    }

    fn has(&self, tool: &str) -> bool {
        self.tools.contains_key(tool)
    }

    fn accepts(&self, tool: &str, argument: &str) -> bool {
        self.tools
            .get(tool)
            .and_then(|tool| tool.input_schema.get("properties"))
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key(argument))
    }

    async fn call(&self, tool: &str, arguments: Map<String, Value>) -> Result<Value, TrackerError> {
        if !self.has(tool) {
            return Err(TrackerError::new(format!(
                "the Linear MCP server does not offer the {tool} tool"
            )));
        }
        if let Some(argument) = arguments
            .keys()
            .find(|argument| !self.accepts(tool, argument))
        {
            return Err(TrackerError::new(format!(
                "Linear's {tool} tool does not accept {argument}; the integration needs updating"
            )));
        }
        let params = CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments);
        let result = timeout(TOOL_TIMEOUT, self.client.call_tool(params))
            .await
            .map_err(|_| TrackerError::new("Linear did not answer in time"))?
            .map_err(|_| TrackerError::new("the Linear connection dropped"))?;
        tool_result(result)
    }
}

pub struct Linear {
    profile: PathBuf,
    session: Mutex<Option<Arc<Session>>>,
}

impl fmt::Debug for Linear {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Linear")
            .field("profile", &self.profile)
            .field("connected", &self.is_connected())
            .finish()
    }
}

impl Linear {
    /// A client for the host-only profile directory at `directory`.
    ///
    /// No credential file is read or required: the bridge owns sign-in, and the
    /// session stays disconnected until `connect` is called. The directory is
    /// created if absent so the bridge has a fixed, private cwd.
    pub fn from_directory(directory: &Path) -> Result<Self, TrackerError> {
        private_directory(directory).map_err(|error| {
            TrackerError::new(format!(
                "could not prepare the Linear profile directory {}: {error}",
                directory.display()
            ))
        })?;
        Ok(Self {
            profile: directory.to_owned(),
            session: Mutex::new(None),
        })
    }

    fn session(&self) -> Option<Arc<Session>> {
        let session = self
            .session
            .try_lock()
            .ok()
            .and_then(|guard| guard.clone())?;
        (!session.client.is_closed()).then_some(session)
    }

    fn has_saved_auth(&self) -> bool {
        let store = self.profile.join(BRIDGE_STORE);
        let Ok(entries) = fs::read_dir(&store) else {
            return false;
        };
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with("_tokens.json")
        })
    }

    /// A session for an operation, reconnecting only when a saved sign-in
    /// exists. With none, the operator is pointed at Settings instead of a
    /// browser being opened from a background call.
    async fn session_or_reconnect(&self) -> Result<Arc<Session>, TrackerError> {
        if let Some(session) = self.session() {
            return Ok(session);
        }
        if !self.has_saved_auth() {
            return Err(TrackerError::new("Connect Linear in Settings → Trackers"));
        }
        self.connect_impl().await?;
        self.session()
            .ok_or_else(|| TrackerError::new("the Linear connection did not stay up"))
    }

    async fn connect_impl(&self) -> Result<(), TrackerError> {
        let mut slot = self.session.lock().await;
        if slot
            .as_ref()
            .is_some_and(|session| !session.client.is_closed())
        {
            return Ok(());
        }
        // A session whose service loop has ended is no longer a connection, so
        // it is dropped before a fresh one is started.
        *slot = None;
        // The lock is held across startup so concurrent callers share one
        // bridge rather than racing to spawn two.
        let session = timeout(CONNECT_TIMEOUT, Session::start(&self.profile))
            .await
            .map_err(|_| TrackerError::new("the Linear sign-in did not finish in time"))??;
        *slot = Some(Arc::new(session));
        Ok(())
    }

    /// Refuse to act when the bound reference and the fetched issue are in
    /// different workspaces.
    ///
    /// The stored slug guards against a reference moving between workspaces;
    /// the fetched URL is the live fact. Both have to name the same workspace.
    fn ensure_workspace(
        &self,
        reference: &IssueReference,
        issue_url: &str,
    ) -> Result<(), TrackerError> {
        let fetched = workspace_slug_from_url(issue_url).ok_or_else(|| {
            TrackerError::new("the Linear server returned an issue without a usable URL")
        })?;
        if reference.workspace_id != fetched {
            return Err(TrackerError::new(
                "this issue belongs to a different Linear workspace than this connection",
            ));
        }
        Ok(())
    }

    async fn search_impl(&self, query: &str) -> Result<Vec<IssueSummary>, TrackerError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(TrackerError::new(
                "enter a Linear issue, a Linear issue URL, or search terms",
            ));
        }
        let session = self.session_or_reconnect().await?;

        if query.starts_with("https://") || query.starts_with("http://") {
            let identifier = issue_identifier_from_url(query)
                .ok_or_else(|| TrackerError::new("paste a Linear issue URL, not another page"))?;
            let issue = fetch_issue(&session, &identifier).await?;
            let fetched = workspace_slug_from_url(&issue_url(&issue)?);
            if fetched != workspace_slug_from_url(query) {
                return Err(TrackerError::new(
                    "that link belongs to a different Linear workspace than this connection",
                ));
            }
            return Ok(vec![summary_of(&issue)?]);
        }

        if looks_like_identifier(query) {
            let issue = fetch_issue(&session, &query.to_ascii_uppercase()).await?;
            if !issue.is_null() {
                return Ok(vec![summary_of(&issue)?]);
            }
        }

        let mut arguments = Map::new();
        arguments.insert("query".to_owned(), json!(query));
        arguments.insert("limit".to_owned(), json!(SEARCH_LIMIT));
        let data = session.call(LIST_ISSUES, arguments).await?;
        let issues = issue_list(&data)?;
        let mut results = Vec::with_capacity(issues.len());
        for issue in &issues {
            results.push(summary_of(issue)?);
        }
        rank(&mut results, query);
        Ok(results)
    }

    async fn read_impl(&self, reference: &IssueReference) -> Result<IssueSnapshot, TrackerError> {
        let session = self.session_or_reconnect().await?;
        let issue = fetch_issue(&session, &reference.id).await?;
        let url = issue_url(&issue)?;
        self.ensure_workspace(reference, &url)?;
        let comments = read_comments(&session, &reference.id).await?;
        Ok(IssueSnapshot {
            issue: summary_of(&issue)?,
            description: issue_description(&issue),
            comments,
        })
    }

    async fn status_impl(
        &self,
        reference: &IssueReference,
        status: TrackerStatus,
    ) -> Result<(), TrackerError> {
        let session = self.session_or_reconnect().await?;
        let issue = fetch_issue(&session, &reference.id).await?;
        self.ensure_workspace(reference, &issue_url(&issue)?)?;
        let state_id = choose_state(&session, &issue, status).await?;
        let mut arguments = Map::new();
        arguments.insert("id".to_owned(), json!(reference.id));
        arguments.insert("state".to_owned(), json!(state_id));
        let result = session.call(SAVE_ISSUE, arguments).await?;
        if result.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(TrackerError::new("Linear did not apply the status change"));
        }
        Ok(())
    }

    async fn comment_impl(
        &self,
        reference: &IssueReference,
        body: &str,
    ) -> Result<(), TrackerError> {
        if body.trim().is_empty() {
            return Err(TrackerError::new(
                "refusing to post an empty Linear comment",
            ));
        }
        let session = self.session_or_reconnect().await?;
        let issue = fetch_issue(&session, &reference.id).await?;
        self.ensure_workspace(reference, &issue_url(&issue)?)?;

        // The MCP comment tool offers no client-chosen id, so a retry after a
        // lost response cannot be made idempotent by request. The existing
        // discussion is checked for this exact body first, which is the one
        // safe approximation available; it can still duplicate if the operator
        // posted identical text.
        let existing = read_comments(&session, &reference.id).await?;
        if existing.iter().any(|comment| comment.body == body) {
            return Ok(());
        }
        let mut arguments = Map::new();
        arguments.insert("issueId".to_owned(), json!(reference.id));
        arguments.insert("body".to_owned(), json!(body));
        let result = session.call(SAVE_COMMENT, arguments).await?;
        if result.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(TrackerError::new("Linear did not post the comment"));
        }
        Ok(())
    }
}

impl TrackerProvider for Linear {
    fn connect<'a>(&'a self) -> TrackerFuture<'a, ()> {
        Box::pin(self.connect_impl())
    }

    fn is_connected(&self) -> bool {
        self.session().is_some()
    }

    fn search<'a>(&'a self, query: &'a str) -> TrackerFuture<'a, Vec<IssueSummary>> {
        Box::pin(self.search_impl(query))
    }

    fn read<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, IssueSnapshot> {
        Box::pin(self.read_impl(reference))
    }

    fn status<'a>(
        &'a self,
        reference: &'a IssueReference,
        status: TrackerStatus,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(self.status_impl(reference, status))
    }

    fn comment<'a>(
        &'a self,
        reference: &'a IssueReference,
        body: &'a str,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(self.comment_impl(reference, body))
    }
}

fn bridge_command(profile: &Path) -> Command {
    let mut command = Command::new("npx");
    command
        .arg("--yes")
        .arg(BRIDGE_PACKAGE)
        .arg(LINEAR_MCP_URL)
        .arg("--transport")
        .arg("http-only")
        .arg("--auth-timeout")
        .arg("120")
        .arg("--silent")
        .env("MCP_REMOTE_CONFIG_DIR", profile)
        .current_dir(profile);
    command
}

fn tool_result(result: CallToolResult) -> Result<Value, TrackerError> {
    if result.is_error == Some(true) {
        return Err(TrackerError::new(tool_error_text(&result)));
    }
    if let Some(value) = result.structured_content {
        return Ok(value);
    }
    for block in &result.content {
        if let ContentBlock::Text(text) = block
            && let Ok(value) = serde_json::from_str::<Value>(&text.text)
        {
            return Ok(value);
        }
    }
    Err(TrackerError::new("the Linear server returned no result"))
}

fn tool_error_text(result: &CallToolResult) -> String {
    let mut message = String::from("Linear rejected the request");
    if let Some(text) = result.content.iter().find_map(ContentBlock::as_text) {
        let detail: String = text.text.chars().take(200).collect();
        if !detail.trim().is_empty() {
            message.push_str(": ");
            message.push_str(detail.trim());
        }
    }
    message
}

async fn fetch_issue(session: &Session, id: &str) -> Result<Value, TrackerError> {
    let mut arguments = Map::new();
    arguments.insert("id".to_owned(), json!(id));
    match session.call(GET_ISSUE, arguments).await {
        Ok(value) => Ok(issue_object(value)),
        Err(error) if is_not_found(&error) => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

fn issue_object(value: Value) -> Value {
    match value {
        Value::Object(mut map) => match map.remove("issue") {
            Some(issue) => issue,
            None => Value::Object(map),
        },
        other => other,
    }
}

fn issue_list(value: &Value) -> Result<Vec<Value>, TrackerError> {
    let array = match value {
        Value::Array(items) => items,
        Value::Object(map) => map
            .get("issues")
            .and_then(Value::as_array)
            .ok_or_else(|| TrackerError::new("the Linear issue list had an unexpected shape"))?,
        _ => {
            return Err(TrackerError::new(
                "the Linear issue list had an unexpected shape",
            ));
        }
    };
    Ok(array.clone())
}

fn issue_url(issue: &Value) -> Result<String, TrackerError> {
    issue
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TrackerError::new("the Linear server returned an issue without a URL"))
}

fn issue_description(issue: &Value) -> String {
    issue
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn summary_of(issue: &Value) -> Result<IssueSummary, TrackerError> {
    let id = string_field(issue, "id")?;
    let identifier = string_field(issue, "identifier")?;
    let url = issue_url(issue)?;
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
    })
}

fn string_field(issue: &Value, field: &str) -> Result<String, TrackerError> {
    issue
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TrackerError::new(format!("the Linear issue had no {field}")))
}

async fn read_comments(
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

/// One page of comments plus the cursor for the next page, when there is one.
///
/// The server may return a full array with no pagination fields, in which case
/// there is no next page.
fn comment_page(value: &Value) -> Result<(Vec<IssueComment>, Option<String>), TrackerError> {
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

fn comment_of(node: &Value) -> Result<IssueComment, TrackerError> {
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

/// Pick the team workflow state a [`TrackerStatus`] means, by type and name.
///
/// Linear teams name and order their own states, so this chooses among what the
/// team actually has and refuses when nothing matches rather than guessing.
async fn choose_state(
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

fn status_list(value: &Value) -> Result<Vec<Value>, TrackerError> {
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

/// The team an issue belongs to, accepting the id, key, or team object the
/// server may return.
fn team_reference(issue: &Value) -> Option<&str> {
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

fn state_name(state: &Value) -> &str {
    state
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn state_kind(state: &Value) -> Option<&str> {
    state
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| state.get("kind").and_then(Value::as_str))
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

fn rank(results: &mut [IssueSummary], term: &str) {
    let term = term.to_ascii_lowercase();
    results.sort_by_cached_key(|summary| {
        let identifier = summary.reference.identifier.to_ascii_lowercase();
        let title = summary.title.to_ascii_lowercase();
        if identifier == term {
            0
        } else if title == term {
            1
        } else if title.starts_with(&term) {
            2
        } else if title.contains(&term) {
            3
        } else {
            4
        }
    });
}

/// The workspace slug in a Linear issue URL, which is the canonical URL's first
/// path segment.
fn workspace_slug_from_url(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    url.path_segments()?.next().map(str::to_owned)
}

/// The identifier in a Linear issue URL, if the URL is one.
///
/// Linear appends view segments to the same resource (`/issue/ENG-1/slug`), and
/// a reported URL may carry them; they do not identify a different issue.
fn issue_identifier_from_url(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("linear.app") {
        return None;
    }
    let parts: Vec<_> = url
        .path_segments()?
        .filter(|part| !part.is_empty())
        .collect();
    let [_workspace, "issue", identifier, ..] = parts.as_slice() else {
        return None;
    };
    looks_like_identifier(identifier).then(|| identifier.to_ascii_uppercase())
}

fn looks_like_identifier(value: &str) -> bool {
    let Some((key, number)) = value.rsplit_once('-') else {
        return false;
    };
    !key.is_empty()
        && key.len() <= 10
        && key.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && key.chars().all(|c| c.is_ascii_alphanumeric())
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

fn is_not_found(error: &TrackerError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("not found")
        || message.contains("could not find")
        || message.contains("does not exist")
}
