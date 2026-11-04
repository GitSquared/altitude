use std::path::Path;

use super::guest::{opener_script, project_scripts, run_setup, run_to_end};
use super::*;
use crate::isolation::{install_script, with_port_relay};

use crate::{
    config::Config,
    project::{Project, ProjectCommits},
};

use super::{
    BaseSandboxSnapshot, CredentialCheck, CredentialReport, CredentialSandbox,
    EnvironmentProgressSink, IsolationEngine, IsolationError, PORT_RELAY_GUEST_PORT,
};

pub async fn ensure_project(
    engine: &impl IsolationEngine,
    config: &Config,
    project: Project,
    recipes: Vec<ProfileRecipe>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<BaseSandboxSnapshot, IsolationError> {
    let mut plan = SandboxPlan::compose(config, project, recipes)?;
    plan.initialize(config)?;
    engine.ensure_base(plan.recipe, progress).await
}

pub(crate) async fn credential_reports(
    engine: &impl IsolationEngine,
    config: &Config,
    sandbox: CredentialSandbox,
    checks: Vec<CredentialCheck>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<Option<Vec<(String, CredentialReport)>>, IsolationError> {
    let CredentialSandbox {
        id,
        recipes,
        snapshot,
        vsock: _,
    } = sandbox;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let plan = SandboxPlan::compose(config, Project::profile_build(), recipes)?;
    // One command and nothing else, so it is sized like the provider watcher
    // rather than like a task.
    create(
        engine,
        config,
        id.clone(),
        plan,
        SandboxKind::Shell,
        Some(SnapshotSource::Environment(snapshot)),
        progress,
    )
    .await?;
    let mut reports = Vec::with_capacity(checks.len());
    for check in checks {
        match run_to_end(engine, id.clone(), check.command).await {
            Ok((status, output)) => reports.push((
                check.profile.clone(),
                CredentialReport {
                    output,
                    signed_in: status == 0,
                },
            )),
            Err(error) => {
                if let Err(error) = engine.remove(id.clone()).await {
                    tracing::warn!(sandbox_id = %id, %error, "could not remove a sandbox after a check");
                }
                return Err(error);
            }
        }
    }
    if let Err(error) = engine.remove(id.clone()).await {
        tracing::warn!(sandbox_id = %id, %error, "could not remove a sandbox after a check");
    }
    Ok(Some(reports))
}

pub(crate) async fn credential_shell(
    engine: &impl IsolationEngine,
    config: &Config,
    sandbox: CredentialSandbox,
    profile: &str,
    progress: &dyn EnvironmentProgressSink,
) -> Result<String, IsolationError> {
    let CredentialSandbox {
        id,
        recipes,
        snapshot,
        vsock,
    } = sandbox;
    let command = recipes
        .iter()
        .find(|recipe| recipe.profile == profile)
        .map(|recipe| recipe.sign_in_command.command())
        .ok_or_else(|| IsolationError::new(format!("no recipe for the {profile} profile")))?;
    let mut plan = SandboxPlan::compose(config, Project::profile_build(), recipes)?;
    let relay = vsock
        .iter()
        .any(|route| route.guest_port == PORT_RELAY_GUEST_PORT);
    plan.options.vsock = vsock;
    create(
        engine,
        config,
        id,
        plan,
        SandboxKind::Shell,
        snapshot.map(SnapshotSource::Environment),
        progress,
    )
    .await?;
    if !relay {
        return Ok(command.to_owned());
    }
    Ok(with_port_relay(command))
}

pub(crate) async fn watcher(
    engine: &impl IsolationEngine,
    config: &Config,
    id: String,
    project: Project,
    recipes: Vec<ProfileRecipe>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<(), IsolationError> {
    let plan = SandboxPlan::compose(config, project, recipes)?;
    // This sandbox runs a provider's read command and nothing else, so it is
    // sized for one command rather than for an agent.
    create(engine, config, id, plan, SandboxKind::Shell, None, progress).await
}

pub(crate) async fn create(
    engine: &impl IsolationEngine,
    config: &Config,
    id: String,
    mut plan: SandboxPlan,
    kind: SandboxKind,
    snapshot: Option<SnapshotSource>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<(), IsolationError> {
    plan.initialize(config)?;
    let path = match snapshot {
        Some(SnapshotSource::Capsule(path)) => path,
        Some(SnapshotSource::Environment(path)) => path,
        None => engine
            .ensure_base(plan.recipe, progress)
            .await?
            .path
            .to_string_lossy()
            .into_owned(),
    };
    let relay = plan
        .options
        .vsock
        .iter()
        .any(|route| route.guest_port == PORT_RELAY_GUEST_PORT);
    plan.options.snapshot_path = Some(path);
    engine
        .create(id.clone(), plan.options, kind.purpose())
        .await?;
    let mut command = String::from("set -eu\n");
    command.push_str(&opener_script());
    if relay {
        command.push_str(&install_script());
    }
    let root = kind.state_root();
    for (source, target) in plan.local_mounts {
        let source = shell_quote(&Path::new(root).join(source).to_string_lossy());
        let target = shell_quote(&target);
        command.push_str(&format!(
            "mkdir -p {source} {target}\nmount --bind {source} {target}\n"
        ));
    }
    if kind == SandboxKind::Task {
        command.push_str(&project_scripts(&plan.project)?);
    }
    if let Err(error) = run_setup(engine, id.clone(), command).await {
        if let Err(cleanup_error) = engine.remove(id).await {
            tracing::warn!(%cleanup_error, "could not remove sandbox after its directory setup failed");
        }
        return Err(error);
    }
    Ok(())
}

pub(crate) fn add_commits_env(environment: &mut Vec<(String, String)>, commits: &ProjectCommits) {
    let name = commits.name.trim();
    let email = commits.email.trim();
    if name.is_empty() || email.is_empty() {
        return;
    }
    for variable in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
        environment.push((variable.to_owned(), name.to_owned()));
    }
    for variable in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
        environment.push((variable.to_owned(), email.to_owned()));
    }
    let mut pairs = vec![("user.name", name), ("user.email", email)];
    if let Some(key) = commits
        .signing_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        pairs.push(("user.signingkey", key));
        pairs.push(("commit.gpgsign", "true"));
    } else {
        // A project that names its own identity and no key does not inherit the
        // login's signing setting by accident.
        pairs.push(("commit.gpgsign", "false"));
    }
    environment.push(("GIT_CONFIG_COUNT".to_owned(), pairs.len().to_string()));
    for (index, (key, value)) in pairs.into_iter().enumerate() {
        environment.push((format!("GIT_CONFIG_KEY_{index}"), key.to_owned()));
        environment.push((format!("GIT_CONFIG_VALUE_{index}"), value.to_owned()));
    }
}
