//! What a task is work on: the code it changes, the tools it needs, and the state
//! that outlives one task.
//!
//! A project is configured in `config.toml` and recorded on every task it founds,
//! so a task that is transferred keeps the setup, repository, and shared caches it
//! was started with. Storage is keyed by the id; the name is what an operator
//! reads, and renaming one changes nothing about the work in flight.

use crate::agent::AgentKind;
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fmt,
    path::{Component, Path},
};
use uuid::Uuid;

/// The caches a new project starts with.
///
/// Paths rather than tools: these are where cargo, npm, pip, uv, go, and pnpm
/// already write, so the common ecosystems are warm without configuring anything.
/// A project that needs none of them removes them, and any other tool's cache is
/// one more path.
pub const DEFAULT_CACHES: &[&str] = &[
    "~/.cargo",
    "~/.cache",
    "~/.npm",
    "~/.local/share/pnpm",
    "~/go/pkg/mod",
];

/// A piece of work, and the machine state it is done in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Project {
    /// Stable across renames, and what storage and tasks are keyed by.
    pub id: String,
    pub name: String,
    /// What a task's machine installs on top of the base image.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub setup_command: String,
    /// The repository a task starts from, when the project has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Directories shared by every task in this project on this machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caches: Vec<String>,
    /// What the operator wants known about this work, given to the agent as part
    /// of its system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The harness a task here runs unless one is picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    /// The machine this project's tasks run on. Absent is this machine.
    ///
    /// A task does not choose: work goes where its project runs, so moving a
    /// project is moving its work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The tracker work this project claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker: Option<TrackerRoute>,
}

/// Work arriving from a task tracker, and what it may do here.
///
/// A route is a claim: this project, and only this one, is what issues from that
/// scope become. Without one, a delegated issue starts nothing and says so.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TrackerRoute {
    /// The provider's scope, by id or by the name a person types: a Linear team, a
    /// Jira project, a GitHub repository.
    ///
    /// Absent claims the whole account, which is what a workspace with one team
    /// needs and what keeps this from being a field about Linear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Whether a delegated issue may start work without an operator at the
    /// keyboard.
    ///
    /// Off until the operator says otherwise. The route alone says which scope the
    /// project is for; this says whether Altitude may act on it unattended.
    #[serde(default)]
    pub accepts_delegated_work: bool,
}

impl TrackerRoute {
    /// Whether this route claims exactly the scope an issue named.
    pub fn claims(&self, id: &str, aliases: &[String]) -> bool {
        let Some(scope) = &self.scope else {
            return false;
        };
        scope.eq_ignore_ascii_case(id)
            || aliases
                .iter()
                .any(|alias| scope.eq_ignore_ascii_case(alias))
    }

    /// Whether this route claims every scope of its account.
    pub fn claims_anything(&self) -> bool {
        self.scope.is_none()
    }

    /// The key two routes are compared by, for refusing an ambiguous pair.
    pub fn claim_key(&self) -> String {
        self.scope
            .as_ref()
            .map(|scope| scope.to_ascii_lowercase())
            .unwrap_or_default()
    }
}

impl Project {
    /// A project with nothing configured, and the caches that make it useful.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            setup_command: String::new(),
            repository: None,
            caches: DEFAULT_CACHES
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
            instructions: None,
            agent: None,
            host: None,
            tracker: None,
        }
    }

    /// A build that belongs to no project: one CLI's own tools, and nothing else.
    ///
    /// A sign-in and an observer need a sandbox but not a project's code, setup, or
    /// caches. They share this shape so they also share a build, and so neither is
    /// recorded as work done in whatever project a task happens to run in.
    pub fn profile_only(name: &str) -> Self {
        Self {
            id: name.to_owned(),
            name: name.to_owned(),
            setup_command: String::new(),
            repository: None,
            caches: Vec::new(),
            instructions: None,
            agent: None,
            host: None,
            tracker: None,
        }
    }

    /// The key two names are compared by.
    ///
    /// Case-insensitive because a tab bar and a settings list are read by a
    /// person, and two entries that differ only in case read as one project.
    pub fn key(&self) -> String {
        self.name.to_ascii_lowercase()
    }

    /// The directory the repository is cloned into, when there is one.
    ///
    /// Derived from the repository rather than configured: one less thing to
    /// keep in step with the URL, and the same place every time a task starts.
    pub fn checkout_path(&self) -> Option<String> {
        self.repository_name().map(|name| format!("~/{name}"))
    }

    /// The repository's last path segment, which names its directory.
    fn repository_name(&self) -> Option<String> {
        let repository = self.repository.as_deref()?;
        repository
            .rsplit(['/', ':'])
            .find(|segment| !segment.is_empty())
            .map(str::to_owned)
    }

    /// What the agent is told about the project at the start of a turn.
    ///
    /// Written into the sandbox rather than sent in every turn, so the prompt
    /// stays about the work and the detail is one path away.
    pub fn brief(&self, checkout: Option<&str>) -> String {
        let mut brief = format!("# Project: {}\n\n", self.name);
        if let (Some(repository), Some(checkout)) = (&self.repository, checkout) {
            brief.push_str(&format!(
                "Altitude cloned the project's repository into this sandbox before \
                 the first turn, and the checkout is part of this task's capsule.\n\n\
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
                "\n## Shared caches\n\nThese directories are shared with every task in \
                 this project on this machine. Another task may be using them, so read \
                 and write them freely and never clear them.\n\n",
            );
            brief.push_str(&self.caches.join("\n"));
            brief.push('\n');
        }
        brief
    }

    pub fn validate(&self) -> Result<(), ProjectError> {
        // A name is read in a tab and in a sandbox label, never used as a path, so
        // it can be the words an operator would write. Only what would break a
        // line or split a path is refused.
        let trimmed = self.name.trim();
        if trimmed.is_empty()
            || trimmed.len() > 64
            || trimmed != self.name
            || self
                .name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err(ProjectError(format!(
                "project name {:?} must be 1-64 printable characters with no leading or trailing space and no slashes",
                self.name
            )));
        }
        if let Some(repository) = &self.repository {
            normalize_repository(repository)?;
        }
        if let Some(route) = &self.tracker
            && let Some(scope) = &route.scope
        {
            // Provider-neutral: a team key, a project key, or `owner/repo`. What a
            // scope may look like belongs to the adapter; what breaks matching is
            // whitespace, so that is what this refuses.
            let scope = scope.trim();
            if scope.is_empty()
                || scope.len() > 128
                || scope != route.scope.as_deref().unwrap_or_default()
                || scope.starts_with('-')
                || scope
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
            {
                return Err(ProjectError(format!(
                    "tracker scope {:?} must be 1-128 printable characters with no spaces, as the tracker names it",
                    route.scope.as_deref().unwrap_or_default()
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

/// The canonical form of a repository, so the same repository written two ways is
/// one string.
///
/// A trailing slash and a `.git` suffix name the same repository as the URL
/// without them, and a task's history should not record which spelling was typed.
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

/// A cache path is a place inside the sandbox, written the way it is reached.
fn validate_cache(value: &str) -> Result<(), ProjectError> {
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
