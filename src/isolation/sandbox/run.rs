use std::path::Path;

use tokio::task::spawn_blocking;
use tokio::time;

use super::guest::{opener_script, project_scripts, project_setup, run_setup, run_to_end};
use super::*;
use crate::isolation::{SandboxBind, install_script, with_port_relay};

use crate::{
    config::Config,
    project::{Project, ProjectCommits},
};

use std::path::PathBuf;

use super::{
    BaseSandboxSnapshot, CredentialSandbox, EnvironmentProgressSink, IsolationEngine,
    IsolationError, PORT_RELAY_GUEST_PORT,
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
    let view = plan.layer_view();
    engine.ensure_base(plan.layers, view, progress).await
}

// A session is renewed by using its harness once. The sandbox is the only place a model
// can be reached, so this is where a renewal happens — on whichever machine can make one.
// Signal everything in the sandbox, including the shell that is doing the signalling, and
// wait for the tree to go quiet. A killed session is a success here: what matters is that
// nothing is writing when the archive starts.
pub(crate) async fn stop_work(
    engine: &impl IsolationEngine,
    id: String,
) -> Result<(), IsolationError> {
    let _ = run_to_end(
        engine,
        id.clone(),
        "kill -TERM -1 2>/dev/null || true".to_owned(),
    )
    .await;
    time::sleep(Duration::from_secs(2)).await;
    Ok(())
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

// A session is renewed by the harness that owns it, and a harness lives in a sandbox when
// the machine holding the profile has none: a normal remote builds one, a workers remote
// spawns a worker, and only the session files come back out. Nothing is mounted in, so the
// refresh runs against the profile the sandbox already carries.
pub(crate) async fn renew_session(
    engine: &impl IsolationEngine,
    config: &Config,
    sandbox: CredentialSandbox,
    command: String,
    take_back: Vec<(String, PathBuf)>,
    progress: &dyn EnvironmentProgressSink,
) -> Result<(), IsolationError> {
    let CredentialSandbox {
        id,
        recipes,
        snapshot,
        vsock,
    } = sandbox;
    let mut plan = SandboxPlan::compose(config, Project::profile_build(), recipes)?;
    plan.options.vsock = vsock;
    create(
        engine,
        config,
        id.clone(),
        plan,
        SandboxKind::Shell,
        snapshot.map(SnapshotSource::Environment),
        progress,
    )
    .await?;
    let used = run_to_end(engine, id.clone(), command).await;
    if used.is_err() {
        tracing::warn!(%id, "the harness could not be used to renew its session");
    }
    for (guest, host) in take_back {
        if let Err(error) = engine.collect_file(id.clone(), guest, host.clone()).await {
            tracing::warn!(%error, path = %host.display(), "a session file did not come back out");
        }
    }
    engine.remove(id).await
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
    // The sandbox's own files include the mounts it will see, and one of those can be a
    // filesystem this machine answers for over the link: a write to it is a network call
    // that needs this runtime to be free, so it cannot be made from the runtime's thread.
    let prepared = config.clone();
    let mut prepared_plan = plan;
    plan = spawn_blocking(move || {
        prepared_plan.initialize(&prepared)?;
        Ok::<_, IsolationError>(prepared_plan)
    })
    .await
    .map_err(|error| IsolationError::new(error.to_string()))??;
    tracing::info!(sandbox = %id, "the sandbox's directories are prepared");
    let path = match snapshot {
        Some(SnapshotSource::Capsule(path)) => path,
        Some(SnapshotSource::Environment(path)) => path,
        None => {
            let view = plan.layer_view();
            tracing::info!(sandbox = %id, "ensuring the installs layer");
            let base = engine.ensure_base(plan.layers, view, progress).await?;
            tracing::info!(sandbox = %id, "the installs layer is there");
            base.path.to_string_lossy().into_owned()
        }
    };
    let relay = plan
        .options
        .vsock
        .iter()
        .any(|route| route.guest_port == PORT_RELAY_GUEST_PORT);
    plan.options.snapshot_path = Some(path);
    // The task-local binds travel with the sandbox's options, so a provider whose
    // commands each run in a fresh mount namespace can re-apply them: the setup
    // command runs once and its mounts are gone by the agent's first command. They
    // are kept out of `mounts` because their source is a guest path, not a host one.
    let state_root = kind.state_root();
    let mut local_mounts = String::new();
    for (source, target) in plan.local_mounts {
        let source = Path::new(state_root).join(source);
        plan.options.binds.push(SandboxBind {
            source: source.to_string_lossy().into_owned(),
            target: target.clone(),
        });
        let source = shell_quote(&source.to_string_lossy());
        let target = shell_quote(&target);
        local_mounts.push_str(&format!(
            "mkdir -p {source} {target}\nmount --bind {source} {target}\n"
        ));
    }
    tracing::info!(sandbox = %id, "opening the sandbox");
    engine
        .create(id.clone(), plan.options, kind.purpose())
        .await?;
    tracing::info!(sandbox = %id, "the sandbox is open");
    let mut command = String::from("set -eu\n");
    command.push_str(&opener_script());
    if relay {
        command.push_str(&install_script());
    }
    command.push_str(&local_mounts);
    if kind == SandboxKind::Task {
        command.push_str(&project_scripts(&plan.project)?);
        command.push_str(&project_setup(&plan.project)?);
    }
    tracing::info!(sandbox = %id, "setting up the sandbox's mounts and project");
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
    // Profiles contribute Git settings first; project settings append to that same list.
    let count = environment
        .iter_mut()
        .find(|(name, _)| name == "GIT_CONFIG_COUNT");
    let offset = if let Some((_, value)) = count {
        let offset: usize = value
            .parse()
            .expect("profile Git configuration count must be valid");
        *value = (offset + pairs.len()).to_string();
        offset
    } else {
        environment.push(("GIT_CONFIG_COUNT".to_owned(), pairs.len().to_string()));
        0
    };
    for (index, (key, value)) in pairs.into_iter().enumerate() {
        environment.push((format!("GIT_CONFIG_KEY_{}", offset + index), key.to_owned()));
        environment.push((
            format!("GIT_CONFIG_VALUE_{}", offset + index),
            value.to_owned(),
        ));
    }
}
