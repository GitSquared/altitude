use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use reqwest::Method;
use serde_json::{Value, json};

pub(crate) mod api;
pub(crate) mod automation;
pub(crate) mod host;
pub(crate) mod issues;
pub(crate) mod settings;
pub(crate) mod setup;
mod sign_in;
pub(crate) mod status;
#[cfg(test)]
mod tests;

use crate::isolation::private_directory;
use crate::tracker::{
    ACCOUNT_FILE, IssueReader, IssueReference, IssueSnapshot, IssueSummary, ScopeSummary,
    TrackerAccount, TrackerError, TrackerFuture, TrackerKind, TrackerProvider, TrackerStatus,
};

use self::api::GitHubApi;
use self::issues::{IssueAddress, snapshot, summary_of, text};
use self::sign_in::Grant;

const SEARCH_LIMIT: usize = 25;

pub struct GitHub {
    profile: PathBuf,
    api: GitHubApi,
}

impl fmt::Debug for GitHub {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHub")
            .field("profile", &self.profile)
            .field("signed_in", &self.is_signed_in())
            .finish()
    }
}

impl GitHub {
    pub fn from_directory(directory: &Path) -> Result<Self, TrackerError> {
        private_directory(directory).map_err(|error| {
            TrackerError::new(format!(
                "could not prepare the GitHub profile directory {}: {error}",
                directory.display()
            ))
        })?;
        Ok(Self {
            profile: directory.to_owned(),
            api: GitHubApi::new()?,
        })
    }

    fn token(&self) -> Result<String, TrackerError> {
        Grant::load(&self.profile).map(|grant| grant.token)
    }

    async fn connect_impl(&self) -> Result<(), TrackerError> {
        let grant = Grant::from_cli()?;
        let user = self.api.get(&grant.token, "/user").await?;
        let account = TrackerAccount {
            provider: TrackerKind::GitHub,
            user_id: user
                .get("id")
                .and_then(Value::as_u64)
                .map(|id| id.to_string())
                .ok_or_else(|| TrackerError::new("GitHub returned no id for your account"))?,
            display_name: text(&user, "login")?,
            workspace: None,
        };
        grant.save(&self.profile)?;
        // The account travels to a remote in this profile, beside the grant: it is
        // how a remote tells whose work a delegation is.
        let bytes = serde_json::to_vec_pretty(&account)
            .map_err(|_| TrackerError::new("could not encode the GitHub account"))?;
        sign_in::write_private(
            &self.profile.join(ACCOUNT_FILE),
            &String::from_utf8_lossy(&bytes),
        )
        .map_err(|_| TrackerError::new("could not record the GitHub account"))
    }

    async fn clear_credentials_impl(&self) -> Result<(), TrackerError> {
        if let Err(error) = fs::remove_dir_all(&self.profile)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(TrackerError::new(
                "could not remove the saved GitHub sign-in",
            ));
        }
        private_directory(&self.profile)
            .map_err(|_| TrackerError::new("could not reset the GitHub profile"))
    }

    async fn search_impl(&self, query: &str) -> Result<Vec<IssueSummary>, TrackerError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(TrackerError::new(
                "enter a GitHub issue URL, owner/repo#number, or search terms",
            ));
        }
        let token = self.token()?;
        if let Some(address) = IssueAddress::from_url(query).or_else(|| IssueAddress::parse(query))
        {
            let issue = self.api.get(&token, &address.path()).await?;
            return Ok(vec![summary_of(&issue)?]);
        }
        // The issues of every repository this account works in, rather than GitHub's
        // search, which would answer with the whole of public GitHub.
        let words: Vec<String> = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let issues = self
            .api
            .pages(&token, "/issues?filter=all&state=open&per_page=100")
            .await?;
        let mut results = Vec::new();
        for issue in issues
            .iter()
            .filter(|issue| issue.get("pull_request").is_none())
        {
            let Ok(summary) = summary_of(issue) else {
                continue;
            };
            let haystack =
                format!("{} {}", summary.reference.identifier, summary.title).to_ascii_lowercase();
            if words.iter().all(|word| haystack.contains(word.as_str())) {
                results.push(summary);
            }
            if results.len() == SEARCH_LIMIT {
                break;
            }
        }
        Ok(results)
    }

    async fn scopes_impl(&self) -> Result<Vec<ScopeSummary>, TrackerError> {
        let token = self.token()?;
        let repositories = self
            .api
            .pages(
                &token,
                "/user/repos?affiliation=owner,collaborator,organization_member&sort=pushed&per_page=100",
            )
            .await?;
        repositories
            .iter()
            .filter(|repository| {
                repository.get("has_issues").and_then(Value::as_bool) != Some(false)
            })
            .map(|repository| {
                let full_name = text(repository, "full_name")?;
                Ok(ScopeSummary {
                    id: full_name.clone(),
                    key: full_name,
                    name: text(repository, "name")?,
                })
            })
            .collect()
    }

    async fn read_impl(&self, reference: &IssueReference) -> Result<IssueSnapshot, TrackerError> {
        let address = address_of(reference)?;
        snapshot(&self.api, &self.token()?, &address).await
    }

    async fn assign_to_self_impl(&self, reference: &IssueReference) -> Result<(), TrackerError> {
        let address = address_of(reference)?;
        let token = self.token()?;
        let login = text(&self.api.get(&token, "/user").await?, "login")?;
        self.api
            .send(
                &token,
                Method::POST,
                &format!("{}/assignees", address.path()),
                Some(json!({ "assignees": [login] })),
            )
            .await
            .map(|_| ())
    }

    async fn status_impl(
        &self,
        reference: &IssueReference,
        status: TrackerStatus,
    ) -> Result<(), TrackerError> {
        let address = address_of(reference)?;
        status::write_status(&self.api, &self.token()?, &address, status).await
    }
}

// An issue's URL or `owner/repo#12` is a GitHub issue, and anything else is words to
// search for.
pub(crate) fn looks_like_issue(value: &str) -> bool {
    IssueAddress::from_url(value.trim()).is_some() || IssueAddress::parse(value).is_some()
}

fn address_of(reference: &IssueReference) -> Result<IssueAddress, TrackerError> {
    IssueAddress::parse(&reference.id)
        .ok_or_else(|| TrackerError::new("this is not a GitHub issue this connection can read"))
}

impl IssueReader for GitHub {
    fn read<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, IssueSnapshot> {
        Box::pin(self.read_impl(reference))
    }
}

impl TrackerProvider for GitHub {
    fn connect<'a>(&'a self) -> TrackerFuture<'a, ()> {
        Box::pin(self.connect_impl())
    }

    fn clear_credentials<'a>(&'a self) -> TrackerFuture<'a, ()> {
        Box::pin(self.clear_credentials_impl())
    }

    // Every call is its own request, so there is no session to keep up: a sign-in on
    // disk is a connection.
    fn is_connected(&self) -> bool {
        self.is_signed_in()
    }

    fn is_signed_in(&self) -> bool {
        sign_in::has_grant(&self.profile)
    }

    fn sign_in_changed_at(&self) -> Option<SystemTime> {
        sign_in::grant_changed_at(&self.profile)
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

    fn assign_to_self<'a>(&'a self, reference: &'a IssueReference) -> TrackerFuture<'a, ()> {
        Box::pin(self.assign_to_self_impl(reference))
    }
}
