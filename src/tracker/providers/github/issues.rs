use serde_json::{Value, json};

use crate::tracker::{
    IssueComment, IssueDependency, IssueReference, IssueScope, IssueSnapshot, IssueSummary,
    TrackerError, TrackerKind,
};

use super::api::GitHubApi;

// An issue is named by its repository and number, which is how every REST call and
// every webhook names it: `Satwa/supercal-v2#12`. A node id would survive a transfer,
// but nothing here could address the issue by it without asking GraphQL first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IssueAddress {
    pub(crate) repository: String,
    pub(crate) number: u64,
}

impl IssueAddress {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        let (repository, number) = id.trim().rsplit_once('#')?;
        let (owner, name) = repository.split_once('/')?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        Some(Self {
            repository: repository.to_owned(),
            number: number.parse().ok()?,
        })
    }

    pub(crate) fn from_url(value: &str) -> Option<Self> {
        let url = ::url::Url::parse(value).ok()?;
        if url.host_str() != Some("github.com") {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.collect();
        match segments.as_slice() {
            [owner, name, "issues", number, ..] => Some(Self {
                repository: format!("{owner}/{name}"),
                number: number.parse().ok()?,
            }),
            _ => None,
        }
    }

    pub(crate) fn id(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }

    pub(crate) fn owner(&self) -> &str {
        self.repository
            .split_once('/')
            .map(|(owner, _)| owner)
            .unwrap_or(&self.repository)
    }

    pub(crate) fn path(&self) -> String {
        format!("/repos/{}/issues/{}", self.repository, self.number)
    }

    pub(crate) fn reference(&self, url: String) -> IssueReference {
        IssueReference {
            provider: TrackerKind::GitHub,
            workspace_id: self.owner().to_owned(),
            id: self.id(),
            identifier: self.id(),
            url,
        }
    }
}

pub(crate) fn summary_of(issue: &Value) -> Result<IssueSummary, TrackerError> {
    let url = text(issue, "html_url")?;
    let address = IssueAddress::from_url(&url)
        .ok_or_else(|| TrackerError::new("GitHub returned an issue without a usable URL"))?;
    let name = address
        .repository
        .split_once('/')
        .map(|(_, name)| name.to_owned())
        .unwrap_or_default();
    Ok(IssueSummary {
        title: text(issue, "title")?,
        project: None,
        scope: Some(IssueScope {
            id: address.repository.clone(),
            aliases: vec![name],
            label: format!("GitHub repository {}", address.repository),
        }),
        reference: address.reference(url),
    })
}

pub(crate) async fn snapshot(
    api: &GitHubApi,
    token: &str,
    address: &IssueAddress,
) -> Result<IssueSnapshot, TrackerError> {
    let issue = api.get(token, &address.path()).await?;
    if issue.get("pull_request").is_some() {
        return Err(TrackerError::new(
            "that is a pull request; altitude works from issues",
        ));
    }
    let comments = api
        .pages(token, &format!("{}/comments?per_page=100", address.path()))
        .await?
        .iter()
        .map(|comment| {
            Ok(IssueComment {
                author: author_login(comment),
                body: text(comment, "body").unwrap_or_default(),
                created_at: text(comment, "created_at")?,
            })
        })
        .collect::<Result<Vec<_>, TrackerError>>()?;
    Ok(IssueSnapshot {
        issue: summary_of(&issue)?,
        description: issue
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        comments,
        blocked_by: dependencies(api, token, address).await?,
    })
}

// An issue's blocked-by relations are GraphQL only: REST has no field for them, which
// is why this second call is here at all. `first: 50` is GitHub's own ceiling for the
// connection; a task waiting on more than that is not a dependency to read, it is a
// project to plan.
const BLOCKED_BY_QUERY: &str = "query ($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      blockedBy(first: 50) {
        nodes {
          number
          title
          state
          repository { nameWithOwner }
        }
      }
    }
  }
}";

pub(crate) async fn dependencies(
    api: &GitHubApi,
    token: &str,
    address: &IssueAddress,
) -> Result<Vec<IssueDependency>, TrackerError> {
    let (owner, name) = address
        .repository
        .split_once('/')
        .ok_or_else(|| TrackerError::new("that is not a GitHub issue this connection can read"))?;
    let data = api
        .graphql(
            token,
            BLOCKED_BY_QUERY,
            json!({ "owner": owner, "name": name, "number": address.number }),
        )
        .await?;
    dependencies_of(&data)
}

// A repository the token cannot see answers with no issue rather than an error, and an
// issue with no blockers answers with no nodes: both are "nothing blocks this".
pub(crate) fn dependencies_of(data: &Value) -> Result<Vec<IssueDependency>, TrackerError> {
    let Some(nodes) = data
        .pointer("/repository/issue/blockedBy/nodes")
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    nodes
        .iter()
        .map(|node| {
            let number = node
                .get("number")
                .and_then(Value::as_u64)
                .ok_or_else(|| TrackerError::new("GitHub returned a blocker without a number"))?;
            let repository = node
                .pointer("/repository/nameWithOwner")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    TrackerError::new("GitHub returned a blocker without a repository")
                })?;
            Ok(IssueDependency {
                identifier: format!("{repository}#{number}"),
                title: text(node, "title")?,
                open: text(node, "state")?.eq_ignore_ascii_case("open"),
            })
        })
        .collect()
}

pub(crate) fn author_login(value: &Value) -> String {
    value
        .pointer("/user/login")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned()
}

pub(crate) fn text(value: &Value, field: &str) -> Result<String, TrackerError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TrackerError::new(format!("the GitHub response had no {field}")))
}
