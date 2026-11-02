use crate::{
    agent::AgentKind,
    credentials::{Service, describe},
    vcs::VcsProviderKind,
};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fmt,
    path::{Component, Path},
};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub setup_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caches: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs: Option<VcsProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs_credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker: Option<TrackerBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<ProjectCommits>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectCommits {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    pub name: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TrackerBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default)]
    pub accepts_delegated_work: bool,
}

impl TrackerBinding {
    pub fn claims(&self, id: &str, aliases: &[String]) -> bool {
        let Some(scope) = &self.scope else {
            return false;
        };
        scope.eq_ignore_ascii_case(id)
            || aliases
                .iter()
                .any(|alias| scope.eq_ignore_ascii_case(alias))
    }

    pub fn claims_anything(&self) -> bool {
        self.scope.is_none()
    }

    pub fn claim_key(&self) -> String {
        self.scope
            .as_ref()
            .map(|scope| scope.to_ascii_lowercase())
            .unwrap_or_default()
    }
}

impl Project {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            setup_command: String::new(),
            repository: None,
            // Nothing is shared until the project says what: a cache is a directory
            // written by one project and read by the next, and guessing which ones
            // a project wants is how a build ends up lying about its own state.
            caches: Vec::new(),
            instructions: None,
            agent: None,
            agent_credential: None,
            host: None,
            vcs: None,
            vcs_credential: None,
            tracker: None,
            commits: None,
        }
    }

    pub fn profile_build() -> Self {
        Self::profile_only("credentials")
    }

    pub fn profile_only(name: &str) -> Self {
        Self {
            id: name.to_owned(),
            name: name.to_owned(),
            setup_command: String::new(),
            repository: None,
            caches: Vec::new(),
            instructions: None,
            agent: None,
            agent_credential: None,
            host: None,
            vcs: None,
            vcs_credential: None,
            tracker: None,
            commits: None,
        }
    }

    pub fn vcs_provider(&self) -> VcsProviderKind {
        self.vcs.unwrap_or_else(VcsProviderKind::sole)
    }

    pub fn key(&self) -> String {
        self.name.to_ascii_lowercase()
    }

    pub fn repository_display(&self) -> Option<String> {
        repository_display(self.vcs_provider(), self.repository.as_deref()?)
    }

    pub fn checkout_path(&self) -> Option<String> {
        self.repository_name().map(|name| format!("~/{name}"))
    }

    fn repository_name(&self) -> Option<String> {
        let repository = self.repository.as_deref()?;
        repository
            .rsplit(['/', ':'])
            .find(|segment| !segment.is_empty())
            .map(str::to_owned)
    }

    pub fn brief(&self, checkout: Option<&str>) -> String {
        let mut brief = format!("# Project: {}\n\n", self.name);
        if let (Some(repository), Some(checkout)) = (&self.repository, checkout) {
            brief.push_str(&format!(
                "Altitude cloned the project's repository into this sandbox before \
                 the first turn, and the checkout is part of this task's sandbox.\n\n\
                 repository: {repository}\ncheckout: {checkout}\n"
            ));
        } else {
            brief.push_str("This project names no repository.\n");
        }
        if !self.setup_command.trim().is_empty() {
            brief.push_str(&format!(
                "\n## Setup\n\nThis machine was built with:\n\n```sh\n{}\n```\n\n\
                 Packages installed while a task runs are lost with this sandbox. A tool \
                 that should be here permanently is a project setup change for the \
                 operator; say so instead of installing it.\n",
                self.setup_command.trim_end()
            ));
        }
        if !self.caches.is_empty() {
            brief.push_str(
                "\n## Shared caches\n\nThese are the complete set of directories shared \
                 with every task in this project on this machine. Another task may be \
                 using them, so read and write them freely and never clear them. Anything \
                 not listed here is local to this task and disappears with it.\n\n",
            );
            for cache in &self.caches {
                brief.push_str(&format!("- {cache}\n"));
            }
        }
        brief
    }

    pub fn validate(&self) -> Result<(), ProjectError> {
        // A name is read in a tab and in a sandbox label, never used as a path, so
        // it can be the words an operator would write. It may be empty on a first
        // run, before the card has asked; only what would break a line or split a
        // path is refused.
        let trimmed = self.name.trim();
        if trimmed.len() > 64
            || trimmed != self.name
            || self
                .name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err(ProjectError(format!(
                "project name {:?} must be at most 64 printable characters with no leading or trailing space and no slashes",
                self.name
            )));
        }
        if let Some(repository) = &self.repository {
            normalize_repository(repository)?;
        }
        if let Some(commits) = &self.commits {
            let valid = |value: &str| {
                let trimmed = value.trim();
                !trimmed.is_empty()
                    && trimmed == value
                    && value.len() <= 128
                    && !value.chars().any(char::is_control)
            };
            if !valid(&commits.name) || !valid(&commits.email) {
                return Err(ProjectError(
                    "a project's commit name and email must be one line each, without leading or trailing space"
                        .to_owned(),
                ));
            }
            if let Some(key) = &commits.signing_key
                && (key.trim().is_empty() || key.chars().any(char::is_control))
            {
                return Err(ProjectError(
                    "a project's signing key must be one non-empty line".to_owned(),
                ));
            }
        }
        if let Some(binding) = &self.tracker
            && let Some(scope) = &binding.scope
        {
            // Provider-neutral: a team key, a project key, or `owner/repo`. What a
            // scope may look like belongs to the adapter; what breaks matching is
            // whitespace, so that is what this refuses.
            let scope = scope.trim();
            if scope.is_empty()
                || scope.len() > 128
                || scope != binding.scope.as_deref().unwrap_or_default()
                || scope.starts_with('-')
                || scope
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
            {
                return Err(ProjectError(format!(
                    "tracker scope {:?} must be 1-128 printable characters with no spaces, as the tracker names it",
                    binding.scope.as_deref().unwrap_or_default()
                )));
            }
        }
        for cache in &self.caches {
            validate_cache(cache)?;
        }
        let mut seen: Vec<String> = Vec::new();
        for cache in &self.caches {
            let path = cache.trim().trim_end_matches('/');
            if seen.iter().any(|existing| {
                path.starts_with(&format!("{existing}/"))
                    || existing.starts_with(&format!("{path}/"))
            }) {
                return Err(ProjectError(format!(
                    "cache {cache:?} overlaps another cache in this project"
                )));
            }
            seen.push(path.to_owned());
        }
        Ok(())
    }
}

pub fn repository_display(provider: VcsProviderKind, repository: &str) -> Option<String> {
    let short = short_repository(repository)?;
    let (owner, name) = short.split_once('/')?;
    Some(describe(
        Service::VersionControl(provider),
        Some(owner),
        name,
    ))
}

fn short_repository(repository: &str) -> Option<String> {
    let short = repository
        .strip_prefix("https://github.com/")
        .or_else(|| repository.strip_prefix("http://github.com/"))
        .or_else(|| repository.strip_prefix("git@github.com:"))
        .unwrap_or(repository);
    let short = short.strip_suffix(".git").unwrap_or(short);
    (!short.trim().is_empty()).then(|| short.trim().to_owned())
}

pub fn normalize_repository(value: &str) -> Result<String, ProjectError> {
    let trimmed = value.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let invalid = |why: &str| Err(ProjectError(format!("repository {value:?} {why}")));
    if trimmed.is_empty() {
        return invalid("is empty");
    }
    if trimmed.len() > 512 {
        return invalid("is longer than 512 characters");
    }
    if trimmed
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return invalid("contains whitespace or control characters");
    }
    if trimmed.contains("..") {
        return invalid("contains `..`");
    }
    // A host is part of the answer, so a clone never depends on this machine's git
    // configuration guessing one. What is typed without a scheme gets https.
    let repository = if trimmed.contains("://") || trimmed.starts_with("git@") {
        trimmed.to_owned()
    } else {
        let host = trimmed.split('/').next().unwrap_or_default();
        let segments = trimmed
            .split('/')
            .filter(|segment| !segment.is_empty())
            .count();
        if segments < 3 || (!host.contains('.') && host != "localhost") {
            return invalid("must name a host, as in https://github.com/owner/repo");
        }
        format!("https://{trimmed}")
    };
    let name = repository
        .rsplit(['/', ':'])
        .find(|segment| !segment.is_empty())
        .unwrap_or("");
    if name.is_empty() || name.starts_with('-') {
        return invalid("does not end in a repository name");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_'))
    {
        return invalid("ends in a repository name that is not usable as a directory");
    }
    Ok(repository)
}

pub(crate) fn validate_cache(value: &str) -> Result<(), ProjectError> {
    let trimmed = value.trim().trim_end_matches('/');
    let invalid = |why: &str| Err(ProjectError(format!("cache {value:?} {why}")));
    let rest = if let Some(rest) = trimmed.strip_prefix("~/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix('/') {
        rest
    } else {
        return invalid("must be an absolute path or start with `~/`");
    };
    if rest.is_empty() {
        return invalid("is a bare root");
    }
    if Path::new(rest)
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return invalid("contains `.`, `..`, or an empty segment");
    }
    Ok(())
}

#[derive(Debug)]
pub struct ProjectError(String);

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ProjectError {}
