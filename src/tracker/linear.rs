//! The Linear tracker adapter.
//!
//! [`Linear`] speaks to Linear's official MCP server over streamable HTTP, driven
//! by the official rmcp client. Signing in is Altitude's own OAuth flow, and the
//! grant lives in the tracker's profile directory, which is never mounted into a
//! capsule.

use std::{
    cmp::Ordering,
    collections::HashMap,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::isolation::private_directory;
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool},
    service::RunningService,
};
use serde_json::{Map, Value, json};
use tokio::{
    sync::{Mutex, watch},
    time::{sleep, timeout},
};

use super::{
    ACCOUNT_FILE, IssueComment, IssueReader, IssueReference, IssueScope, IssueSnapshot,
    IssueSummary, ScopeSummary, TrackerAccount, TrackerError, TrackerFuture, TrackerKind,
    TrackerProvider, TrackerStatus, linear_auth,
};

const LIST_ISSUES: &str = "list_issues";
const LIST_TEAMS: &str = "list_teams";
const GET_ISSUE: &str = "get_issue";
const LIST_COMMENTS: &str = "list_comments";
const LIST_ISSUE_STATUSES: &str = "list_issue_statuses";
const SAVE_ISSUE: &str = "save_issue";

const REQUIRED_TOOLS: [&str; 5] = [
    LIST_ISSUES,
    GET_ISSUE,
    LIST_COMMENTS,
    LIST_ISSUE_STATUSES,
    SAVE_ISSUE,
];

const SEARCH_LIMIT: u64 = 25;
const PAGE_LIMIT: u64 = 100;
const MAX_COMMENT_PAGES: usize = 20;
/// Bounds sign-in and startup, which is a browser round trip when nothing is
/// saved.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(180);
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

struct Session {
    client: RunningService<RoleClient, ()>,
    tools: HashMap<String, Tool>,
}

impl Session {
    async fn start(profile: &Path) -> Result<Self, TrackerError> {
        let client = linear_auth::authorized_client(profile).await?;
        let transport = linear_auth::transport(client);
        let mut client = ().serve(transport).await.map_err(|_| {
            TrackerError::new(
                "could not connect to Linear; sign in again from Settings → Task tracker",
            )
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
    /// Bumped when the saved sign-in is forgotten, so a sign-in already under
    /// way stops instead of writing a fresh one after the files are gone.
    generation: watch::Sender<u64>,
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
    /// Nothing is read from it until `connect` is called, and the sign-in it
    /// holds is written there and nowhere else. The directory is created if
    /// absent so the grant has a private place to land.
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
            generation: watch::channel(0).0,
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
        linear_auth::has_grant(&self.profile)
    }

    /// Whether a sign-in is saved, so a machine serving a peer can tell an
    /// account that peer synced from one it never signed in to.
    pub fn is_signed_in(&self) -> bool {
        self.has_saved_auth()
    }

    /// When the saved sign-in last changed, so a machine that opened a client
    /// from it can tell that the login has since been replaced.
    pub fn sign_in_changed_at(&self) -> Option<SystemTime> {
        linear_auth::grant_changed_at(&self.profile)
    }

    /// A session for an operation, reconnecting only when a saved sign-in
    /// exists. With none, the operator is pointed at Settings instead of a
    /// browser being opened from a background call.
    async fn session_or_reconnect(&self) -> Result<Arc<Session>, TrackerError> {
        if let Some(session) = self.session() {
            return Ok(session);
        }
        if !self.has_saved_auth() {
            return Err(TrackerError::new(
                "Connect Linear in Settings → Task tracker",
            ));
        }
        self.connect_impl().await?;
        self.session()
            .ok_or_else(|| TrackerError::new("the Linear connection did not stay up"))
    }

    async fn connect_impl(&self) -> Result<(), TrackerError> {
        // Read the epoch before waiting for the lock: a sign-in that started
        // before the credentials were forgotten must not write a fresh login
        // after them.
        let epoch = *self.generation.borrow();
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
        let mut generation = self.generation.subscribe();
        if *generation.borrow() != epoch {
            return Err(TrackerError::new("the Linear sign-in was cancelled"));
        }
        // The lock is held across startup so concurrent callers share one
        // session rather than racing to open two.
        let session = tokio::select! {
            result = timeout(CONNECT_TIMEOUT, Session::start(&self.profile)) => {
                result
                    .map_err(|_| TrackerError::new("the Linear sign-in did not finish in time"))??
            }
            _ = generation.changed() => {
                return Err(TrackerError::new("the Linear sign-in was cancelled"));
            }
        };
        let session = Arc::new(session);
        *slot = Some(session.clone());
        drop(slot);
        // The account travels to a remote in this profile, so it is written as
        // soon as there is a session that can be asked who this is.
        if let Err(error) = self.record_account(&session).await {
            tracing::warn!(%error, "could not record the Linear account");
        }
        Ok(())
    }

    async fn record_account(&self, session: &Session) -> Result<(), TrackerError> {
        let account = read_account(session).await?;
        let bytes = serde_json::to_vec_pretty(&account)
            .map_err(|_| TrackerError::new("could not encode the Linear account"))?;
        fs::write(self.profile.join(ACCOUNT_FILE), bytes)
            .map_err(|_| TrackerError::new("could not record the Linear account"))
    }

    /// Forget the saved Linear sign-in.
    ///
    /// A sign-in already under way is stopped and the live session ended before
    /// the grant is removed; otherwise a late callback could write a fresh login
    /// back.
    async fn clear_credentials_impl(&self) -> Result<(), TrackerError> {
        self.generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        let mut slot = self.session.lock().await;
        if let Some(mut session) = slot.take() {
            session.client.cancellation_token().cancel();
            let owned = timeout(CLEANUP_TIMEOUT, async {
                loop {
                    match Arc::try_unwrap(session) {
                        Ok(session) => break session,
                        Err(shared) => session = shared,
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .map_err(|_| {
                TrackerError::new("Linear is still closing; try deleting its sign-in again")
            })?;
            let mut client = owned.client;
            match client.close_with_timeout(CLEANUP_TIMEOUT).await {
                Ok(Some(_)) => {}
                _ => {
                    return Err(TrackerError::new(
                        "Linear did not finish closing; the saved sign-in was kept",
                    ));
                }
            }
        }
        // A reconnect requested during cleanup must not open a browser after it.
        self.generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        // The profile holds nothing but the sign-in, so it goes whole and is
        // recreated empty for the next one.
        if let Err(error) = fs::remove_dir_all(&self.profile)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(TrackerError::new(
                "could not remove the saved Linear sign-in",
            ));
        }
        private_directory(&self.profile)
            .map_err(|_| TrackerError::new("could not reset the Linear profile"))
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

    async fn scopes_impl(&self) -> Result<Vec<ScopeSummary>, TrackerError> {
        let session = self.session_or_reconnect().await?;
        if !session.has(LIST_TEAMS) {
            return Err(TrackerError::new(
                "this Linear connection cannot list teams; type the team key instead",
            ));
        }
        let data = session.call(LIST_TEAMS, Map::new()).await?;
        let teams = object_list(&data, "teams")?;
        let mut scopes = Vec::with_capacity(teams.len());
        for team in &teams {
            scopes.push(ScopeSummary {
                id: string_field(team, "id")?,
                key: string_field(team, "key")?,
                name: string_field(team, "name")?,
            });
        }
        Ok(scopes)
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
        let updated = result.get("issue").unwrap_or(&result);
        if result.get("success").and_then(Value::as_bool) == Some(false)
            || (result.get("success").and_then(Value::as_bool) != Some(true)
                && updated.get("id").and_then(Value::as_str) != Some(reference.id.as_str()))
        {
            return Err(TrackerError::new(
                "Linear did not confirm the status change",
            ));
        }
        Ok(())
    }
}

async fn read_account(session: &Session) -> Result<TrackerAccount, TrackerError> {
    let parameter = if session.accepts("get_user", "query") {
        "query"
    } else {
        "id"
    };
    let mut arguments = Map::new();
    arguments.insert(parameter.into(), json!("me"));
    let data = session.call("get_user", arguments).await?;
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
        workspace: workspace_of(user),
    })
}

/// The workspace the account is in, from whichever shape the provider sent.
///
/// Asked of the account rather than fetched separately: it is one fact about one
/// login, and a provider that does not name it leaves it absent rather than
/// guessed.
fn workspace_of(user: &Value) -> Option<String> {
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

impl IssueReader for Linear {
    fn read<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, IssueSnapshot> {
        Box::pin(self.read_impl(reference))
    }
}

impl TrackerProvider for Linear {
    fn connect<'a>(&'a self) -> TrackerFuture<'a, ()> {
        Box::pin(self.connect_impl())
    }

    fn clear_credentials<'a>(&'a self) -> TrackerFuture<'a, ()> {
        Box::pin(self.clear_credentials_impl())
    }

    fn is_connected(&self) -> bool {
        self.session().is_some()
    }

    fn search<'a>(&'a self, query: &'a str) -> TrackerFuture<'a, Vec<IssueSummary>> {
        Box::pin(self.search_impl(query))
    }

    fn scopes<'a>(&'a self) -> TrackerFuture<'a, Vec<ScopeSummary>> {
        Box::pin(self.scopes_impl())
    }

    fn status<'a>(
        &'a self,
        reference: &'a IssueReference,
        status: TrackerStatus,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(self.status_impl(reference, status))
    }
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
    object_list(value, "issues")
}

fn object_list(value: &Value, name: &str) -> Result<Vec<Value>, TrackerError> {
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
        scope: scope_of(issue)?,
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

/// The scope a Linear issue belongs to: its team.
///
/// Absent rather than an error: an issue with no team is one no route can claim.
pub(super) fn scope_of(issue: &Value) -> Result<Option<IssueScope>, TrackerError> {
    let Some(team) = issue.get("team").filter(|team| !team.is_null()) else {
        return Ok(None);
    };
    let id = team
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| TrackerError::new("the tracker returned a scope without an id"))?;
    let key = team.get("key").and_then(Value::as_str).map(str::to_owned);
    let name = team.get("name").and_then(Value::as_str).map(str::to_owned);
    let label = format!(
        "Linear team {}",
        key.as_deref().or(name.as_deref()).unwrap_or(id)
    );
    Ok(Some(IssueScope {
        id: id.to_owned(),
        aliases: key.into_iter().collect(),
        label,
    }))
}
