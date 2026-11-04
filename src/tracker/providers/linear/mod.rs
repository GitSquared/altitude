use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

pub(crate) mod auth;
pub(crate) mod automation;
pub(crate) mod oauth;
pub(crate) mod setup;

use crate::isolation::private_directory;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};
use tokio::{
    sync::{Mutex, watch},
    time::{sleep, timeout},
};

use crate::tracker::{
    ACCOUNT_FILE, IssueReader, IssueReference, IssueScope, IssueSnapshot, IssueSummary,
    ScopeSummary, TrackerError, TrackerFuture, TrackerProvider, TrackerStatus,
};

mod issue;
mod session;
mod status;
mod url;

use self::issue::{
    fetch_issue, issue_description, issue_list, issue_url, object_list, read_account,
    read_comments, string_field, summary_of,
};
use self::session::Session;
use self::status::choose_state;
use self::url::{issue_identifier_from_url, looks_like_identifier, rank, workspace_slug_from_url};

pub(super) const LIST_ISSUES: &str = "list_issues";
pub(super) const LIST_TEAMS: &str = "list_teams";
pub(super) const WHOAMI: &str = "whoami";
pub(super) const GET_ISSUE: &str = "get_issue";
pub(super) const LIST_COMMENTS: &str = "list_comments";
pub(super) const LIST_ISSUE_STATUSES: &str = "list_issue_statuses";
pub(super) const SAVE_ISSUE: &str = "save_issue";

pub(super) const REQUIRED_TOOLS: [&str; 5] = [
    LIST_ISSUES,
    GET_ISSUE,
    LIST_COMMENTS,
    LIST_ISSUE_STATUSES,
    SAVE_ISSUE,
];

pub(super) const SEARCH_LIMIT: u64 = 25;
pub(super) const PAGE_LIMIT: u64 = 100;
pub(super) const MAX_COMMENT_PAGES: usize = 20;
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(180);
pub(super) const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Linear {
    profile: PathBuf,
    session: Mutex<Option<Arc<Session>>>,
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
        auth::has_grant(&self.profile)
    }

    pub fn is_signed_in(&self) -> bool {
        self.has_saved_auth()
    }

    pub fn sign_in_changed_at(&self) -> Option<SystemTime> {
        auth::grant_changed_at(&self.profile)
    }

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
            // A row this client cannot read is a row missing from the list, not
            // an answer of "nothing found": the rest are still worth showing, and
            // the log says what was dropped rather than the page saying nothing.
            match summary_of(issue) {
                Ok(summary) => results.push(summary),
                Err(error) => tracing::warn!(
                    %error,
                    fields = ?issue
                        .as_object()
                        .map(|object| object.keys().cloned().collect::<Vec<_>>()),
                    "could not read a Linear issue from a search"
                ),
            }
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

    async fn assign_to_self_impl(&self, reference: &IssueReference) -> Result<(), TrackerError> {
        let session = self.session_or_reconnect().await?;
        let account = read_account(&session).await?;
        let issue = fetch_issue(&session, &reference.id).await?;
        self.ensure_workspace(reference, &issue_url(&issue)?)?;
        let mut arguments = Map::new();
        arguments.insert("id".to_owned(), json!(reference.id));
        arguments.insert("assignee".to_owned(), json!(account.user_id));
        let result = session.call(SAVE_ISSUE, arguments).await?;
        if result.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(TrackerError::new("Linear did not confirm the assignee"));
        }
        Ok(())
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

    fn assign_to_self<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, ()> {
        Box::pin(self.assign_to_self_impl(reference))
    }

    fn status<'a>(
        &'a self,
        reference: &'a IssueReference,
        status: TrackerStatus,
    ) -> TrackerFuture<'a, ()> {
        Box::pin(self.status_impl(reference, status))
    }
}

pub(super) fn tool_result(result: CallToolResult) -> Result<Value, TrackerError> {
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

pub(super) fn tool_error_text(result: &CallToolResult) -> String {
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

fn is_not_found(error: &TrackerError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("not found")
        || message.contains("could not find")
        || message.contains("does not exist")
}

pub(crate) fn scope_of(issue: &Value) -> Result<Option<IssueScope>, TrackerError> {
    let Some(team) = issue.get("team").filter(|team| !team.is_null()) else {
        return Ok(None);
    };
    // A team is an object in Linear's own API and sometimes only the word the
    // operator knows it by in what a tool returns. What names the team answers
    // for all of it: a scope is matched by name against a project's claim, never
    // used to write anything back.
    let (id, key, name) = match team {
        Value::String(named) => (None, Some(named.clone()), None),
        Value::Object(_) => (
            team.get("id").and_then(Value::as_str).map(str::to_owned),
            team.get("key").and_then(Value::as_str).map(str::to_owned),
            team.get("name").and_then(Value::as_str).map(str::to_owned),
        ),
        other => {
            tracing::debug!(shape = ?other, "the tracker named a team in a shape this client does not read");
            return Ok(None);
        }
    };
    let Some(shown) = key.clone().or_else(|| name.clone()).or_else(|| id.clone()) else {
        tracing::debug!("the tracker returned a team with no name at all");
        return Ok(None);
    };
    let id = id.unwrap_or_else(|| shown.clone());
    let mut aliases: Vec<String> = key.into_iter().collect();
    if id != shown {
        aliases.push(shown.clone());
    }
    Ok(Some(IssueScope {
        aliases,
        label: format!("Linear team {shown}"),
        id,
    }))
}
