//! The serve role: work for clients, with no terminal.
//!
//! A different execution mode, not a different configuration of the same one. A
//! serve role has nobody at the keyboard, so it never takes the terminal, it logs to
//! stdout for whatever service manager started it, and it reads its own settings
//! file rather than the client's.

use std::{error::Error, io, net::SocketAddr};

use std::sync::{Arc, Mutex};

use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

use crate::{
    agent::AgentEngines,
    cli::ServeArgs,
    config::{Config, InstallationIdentity, ServeSettings},
    db::Database,
    isolation::{
        self, EnvironmentProgress, EnvironmentProgressSink, IsolationStatus, MicrosandboxIsolation,
    },
    logging,
    remote::protocol::{TaskRequest, TaskSummary},
    remote::{
        ClientRecord, InstallationKey, LinkServer,
        announce::RemoteState,
        bundle::HeldConfig,
        server::{BoxFuture, ServerParts, TaskHost},
    },
    task::{Task, TaskEventLoop, TaskService, TaskStore},
    vcs::GitHub,
};

/// The tasks this machine owns, as the link asks about them.
///
/// A task belongs to the machine that runs it, so this is not a view of somebody
/// else's work: these are the only copies that exist.
struct OwnTasks {
    config: Arc<Config>,
    tasks: Arc<TaskEventLoop<MicrosandboxIsolation>>,
}

impl TaskHost for OwnTasks {
    fn run(&self, request: TaskRequest) -> BoxFuture<Result<(), String>> {
        let config = Arc::clone(&self.config);
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            let settings = config
                .load_or_create_settings()
                .map_err(|error| error.to_string())?;
            let environment = settings
                .settings()
                .environments()
                .into_iter()
                .find(|environment| environment.name == request.environment)
                .ok_or_else(|| {
                    format!("this machine has no {} environment", request.environment)
                })?;
            tasks.create(
                request.intent,
                environment,
                request.agent,
                Some(InstallationIdentity {
                    id: request.identity_id,
                    label: request.identity_label,
                }),
            );
            Ok(())
        })
    }

    fn list(&self) -> BoxFuture<Result<Vec<TaskSummary>, String>> {
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            let owned = tasks.list().await.map_err(|error| error.to_string())?;
            Ok(owned.iter().map(summarise).collect())
        })
    }
}

/// What a card needs, which is not what a machine needs to run a task.
fn summarise(task: &Task) -> TaskSummary {
    let mut extras = Vec::new();
    if task.facts.unread_operator_signal_count > 0 {
        extras.push(format!(
            "{} unread",
            task.facts.unread_operator_signal_count
        ));
    }
    TaskSummary {
        id: task.id.clone(),
        host: task.host.clone(),
        intent: task.facts.intent.clone(),
        environment: task.facts.environment_name.clone(),
        agent: task.agent_kind,
        status: task.facts.execution_status.clone(),
        mode: task.facts.execution_mode,
        extras,
    }
}

/// Builds every environment the configuration names, and only then says this machine
/// can be given work.
///
/// A remote that answered "ready" while an environment was missing would fail the
/// first task that asked for it. Building here puts that wait before anything is
/// delegated, rather than inside somebody's task.
async fn build_environments(
    config: &Config,
    isolation: MicrosandboxIsolation,
    prepared: &mut watch::Receiver<IsolationStatus>,
    state: &watch::Sender<RemoteState>,
) -> Result<(), Box<dyn Error>> {
    loop {
        match &*prepared.borrow_and_update() {
            IsolationStatus::Ready => break,
            IsolationStatus::Failed(error) => return Err(io::Error::other(error.clone()).into()),
            _ => {}
        }
        if prepared.changed().await.is_err() {
            return Err(io::Error::other("isolation stopped unexpectedly").into());
        }
    }
    let settings = config.load_or_create_settings()?;
    let agents = AgentEngines::new();
    let kind = settings.settings().agent().unwrap_or_default();
    for environment in settings.settings().environments() {
        // The base is the workspace the runtime was prepared with, and every
        // environment is built on top of it. It is not built again here.
        if environment.is_base() {
            continue;
        }
        tracing::info!(environment = %environment.name, "building an environment");
        isolation::ensure_environment(
            &isolation,
            config,
            environment,
            vec![
                agents.engine(kind).environment_recipe(),
                GitHub::environment_recipe(),
            ],
            &Unwatched,
        )
        .await
        .map_err(io::Error::other)?;
    }
    state.send_replace(RemoteState::Ready);
    Ok(())
}

/// Nobody waits on a remote's build: the isolation layer logs its steps, and a task's
/// progress has nothing to do with it.
struct Unwatched;

impl EnvironmentProgressSink for Unwatched {
    fn report(&self, _progress: EnvironmentProgress) {}
}

/// Runs a server until it is asked to stop.
pub async fn run(args: ServeArgs) -> Result<(), Box<dyn Error>> {
    logging::init_service();

    let config = match args.data_dir {
        Some(dir) => Config::load()?.with_data_dir(dir).serve(),
        None => Config::load()?.serve(),
    };
    let settings_path = args.config.unwrap_or_else(|| config.serve_settings_path());
    let settings = ServeSettings::load_or_create(&settings_path)?;
    let listen: SocketAddr = args
        .listen
        .as_deref()
        .unwrap_or(&settings.listen)
        .parse()
        .map_err(|error| {
            io::Error::other(format!(
                "{} is not an address and port: {error}",
                args.listen.as_deref().unwrap_or(&settings.listen)
            ))
        })?;

    let identity = InstallationKey::load_or_create(config.data_dir().join("identity.key"))
        .map_err(io::Error::other)?;
    let clients =
        ClientRecord::open(config.data_dir().join("clients.json")).map_err(io::Error::other)?;
    // The label travels with the key, so a client that learns one learns what to
    // call this remote as well.
    let label = config.identity().map_err(io::Error::other)?.label;
    // A remote is a replica of a setup, so it builds what that setup needs before it
    // can be given work. It answers while it builds, saying so, which is how a client
    // tells a machine that is preparing from one that is down.
    let isolation = MicrosandboxIsolation::new(&config);
    let prepared = isolation::prepare(isolation.clone());
    // This machine's own answer to "can you be given work", which is not the same
    // question as "is the runtime up": it is ready once what the configuration asks
    // for has been built.
    let (state, observed) = watch::channel(RemoteState::Preparing);
    // What this machine holds, kept beside its data so a client can be told what it
    // does not need to send again.
    let held = Arc::new(Mutex::new(HeldConfig::load(
        &config.data_dir().join("profiles.json"),
    )));
    // The tasks this machine owns. Its own database, because a task belongs to the
    // machine that runs it: there is no copy of one anywhere else, and a client that
    // wants to see these reads them from here.
    let database = Database::open(&config).await.map_err(io::Error::other)?;
    let tasks = TaskService::new(
        TaskStore::new(&database),
        isolation.clone(),
        config.task_snapshots_dir(),
        AgentEngines::new(),
        config.identity().map_err(io::Error::other)?,
    );
    let (task_loop, mut updates) = TaskEventLoop::new(tasks);
    let task_loop = Arc::new(task_loop);
    // Nobody is at the keyboard here, so what happens to these tasks is said once, in
    // the log, and nowhere else.
    tokio::spawn(async move {
        while updates.recv().await.is_some() {
            tracing::debug!("a task changed");
        }
    });
    let server = LinkServer::serve(ServerParts {
        identity,
        clients,
        listen,
        label,
        config_path: config.settings_path(),
        state: observed,
        held: Arc::clone(&held),
        tasks: Arc::new(OwnTasks {
            config: Arc::new(config.clone()),
            tasks: task_loop,
        }),
    })
    .await
    .map_err(io::Error::other)?;
    let ticket = server.ticket().map_err(io::Error::other)?;

    // Everything an operator needs to point a client at this remote, said once and
    // said at the place they will be looking.
    tracing::info!(
        listening = %listen,
        settings = %settings_path.display(),
        data = %config.data_dir().display(),
        "serving"
    );
    println!("listening on {listen} (udp)");
    println!("settings     {}", settings_path.display());
    println!("data         {}", config.data_dir().display());
    println!("identity     {}", server.id());
    println!("ticket       {ticket}");

    // Built behind the server, so the answer to "what can you do" is available while
    // the answer is still "not yet".
    tokio::spawn({
        let config = config.clone();
        async move {
            let mut prepared = prepared;
            if let Err(error) = build_environments(&config, isolation, &mut prepared, &state).await
            {
                tracing::error!(%error, "could not build what the configuration asks for");
            }
        }
    });

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = terminate.recv() => tracing::info!("asked to stop"),
        _ = interrupt.recv() => tracing::info!("asked to stop"),
    }

    server.shutdown().await.map_err(io::Error::other)?;
    Ok(())
}
