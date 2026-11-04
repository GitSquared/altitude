use std::path::{Path, PathBuf};

use super::guest::relative_path;
use super::*;

use crate::{config::Config, project::Project};

use super::{
    BaseSandboxRecipe, EnvironmentProgressSink, IsolationEngine, IsolationError, SandboxPurpose,
    VsockRoute,
};

pub(crate) enum SnapshotSource {
    Capsule(String),
    Environment(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxKind {
    Task,
    Shell,
}

impl SandboxKind {
    pub(crate) fn purpose(self) -> SandboxPurpose {
        match self {
            Self::Task => SandboxPurpose::Capsule,
            Self::Shell => SandboxPurpose::Command,
        }
    }

    pub(crate) fn state_root(self) -> &'static str {
        match self {
            Self::Task => "/altitude/task-state",
            Self::Shell => "/tmp/altitude-shell",
        }
    }
}

pub(crate) struct ProvisionRequest {
    pub id: String,
    pub snapshot: Option<String>,
    pub project: Project,
    pub recipes: Vec<ProfileRecipe>,
    pub vsock: Vec<VsockRoute>,
}

pub(crate) async fn provision(
    engine: &impl IsolationEngine,
    config: &Config,
    request: ProvisionRequest,
    progress: &dyn EnvironmentProgressSink,
) -> Result<(), IsolationError> {
    let ProvisionRequest {
        id,
        snapshot,
        project,
        recipes,
        vsock,
    } = request;
    let mut plan = SandboxPlan::compose(config, project, recipes)?;
    plan.options.vsock = vsock;
    create(
        engine,
        config,
        id,
        plan,
        SandboxKind::Task,
        snapshot.map(SnapshotSource::Capsule),
        progress,
    )
    .await
}

// The layer a task runs on: what a sandbox would be shown, without building it.
pub(crate) fn environment_top_layer(
    config: &Config,
    project: Project,
    recipes: Vec<ProfileRecipe>,
) -> Result<Option<BaseSandboxRecipe>, IsolationError> {
    Ok(SandboxPlan::compose(config, project, recipes)?.layers.pop())
}

pub(crate) fn cache_storage_path(cache: &str) -> PathBuf {
    let cache = cache.trim().trim_end_matches('/');
    match cache.strip_prefix("~/") {
        Some(rest) => Path::new("home").join(rest),
        None => Path::new("abs").join(cache.trim_start_matches('/')),
    }
}

pub(crate) fn expand_guest_path(path: &str) -> Result<PathBuf, IsolationError> {
    let path = path.trim().trim_end_matches('/');
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => Path::new(GUEST_HOME).join(rest),
        None if path.starts_with('/') => PathBuf::from(path),
        None => {
            return Err(IsolationError::new(format!(
                "project path {path:?} must be absolute or start with `~/`"
            )));
        }
    };
    relative_path(expanded.to_string_lossy().trim_start_matches('/'), false)?;
    Ok(expanded)
}
