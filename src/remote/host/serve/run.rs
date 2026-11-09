use std::{
    collections::HashMap,
    error::Error,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::brand;
use crate::version::VERSION;

// A machine stopped for host maintenance gives its guest a minute to shut down.
// Announcing the stop, parking every task and closing the links have to fit inside
// that, so the parking half gets the smaller share of it.
const STOP_BUDGET: Duration = Duration::from_secs(45);
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::remote::send::IncomingTransfers;

use tokio::{
    signal::unix::{SignalKind, signal},
    sync::{Notify, watch},
    time::{self, timeout},
};

use crate::{
    agent::{AgentEngines, AgentKind},
    cli::{ServeArgs, ServeCommand, Tracker},
    config::{Config, DEFAULT_PORT},
    db::Database,
    isolation::{self, IsolationStatus, MicrosandboxIsolation, Unwatched},
    logging,
    project::Project,
    remote::protocol::TaskSummary,
    remote::{
        ClientRecord, InstallationKey, LinkServer, TrackerConnection,
        announce::{Phase, TrackerReadiness},
        server::ServerParts,
    },
    task::{TaskEventLoop, TaskExecutionUpdate, TaskService, TaskStore},
    tracker::providers::{github::host as github_host, linear::host as linear_host},
    tracker::{DelegatedWork, TrackerIntegration, TrackerKind},
};

use super::settings::ServeSettings;
use super::tracker_host::PushedTrackers;
use super::{OwnTasks, Transfers, Watchers};
use crate::remote::host::{admin, install};

pub async fn run(args: ServeArgs) -> Result<(), Box<dyn Error>> {
    let base = match &args.data_dir {
        Some(dir) => Config::load()?.with_data_dir(dir.clone()),
        None => Config::load()?,
    };
    let config = base.serve();
    let settings_path = args
        .config
        .clone()
        .unwrap_or_else(|| config.serve_settings_path());
    // Everything below happens instead of serving, and before the log is pointed
    // at stdout: what it prints is the answer, not a report about it.
    if let Some(command) = &args.command {
        match command {
            ServeCommand::Clients => {
                admin::list(&config)
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            ServeCommand::Evict { key } => {
                admin::evict(&config, key)
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            // A service definition carries this machine's own paths, so it is
            // written by the process that has them rather than from a template
            // somebody downloaded.
            ServeCommand::Install => {
                return install::install(&args, &base).await;
            }
            ServeCommand::Uninstall => {
                return install::uninstall(&args, &base).await;
            }
            ServeCommand::Connect(connect) => {
                let settings = ServeSettings::load_or_create(&settings_path)?;
                let credentials_path = linear_host::credentials_path(&config);
                return match connect.tracker {
                    Tracker::Linear => {
                        linear_host::connect_remote(&settings, &settings_path, &credentials_path)
                            .await
                    }
                };
            }
        }
    }

    let settings = ServeSettings::load_or_create(&settings_path)?;
    let credentials_path = linear_host::credentials_path(&config);
    logging::init_service();
    tracing::info!("{}  altitude {VERSION} · remote", brand::MOUNTAIN);
    tracing::debug!("Checking sandbox host requirements");
    MicrosandboxIsolation::check_host()?;
    tracing::debug!("Sandbox host requirements passed");
    // Where it read its settings from, because a secret that was rotated into a
    // file this process is not reading looks exactly like a secret that is wrong.
    tracing::info!(
        settings = %settings_path.display(),
        data = %config.data_dir().display(),
        "Remote configuration"
    );

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

    // A remote reports into one tracker. Linear's app is kept when both are set,
    // because it is the one a remote set up before GitHub existed here was using.
    if settings.tracker.is_some() && settings.github.is_some() {
        tracing::warn!(
            "serve.toml sets both [linear] and [github]; this remote reports into Linear"
        );
    }
    let mut tracker_integration: Option<Arc<dyn TrackerIntegration>> = None;
    let mut tracker_error = None;
    let configured = if let Some(tracker_settings) = settings.tracker.clone() {
        match linear_host::connect(&config, &tracker_settings, &credentials_path).await {
            Ok(runtime) => tracker_integration = Some(runtime),
            // Reported once, below, with what to run about it.
            Err(error) => tracker_error = Some(error),
        }
        Some(TrackerKind::Linear)
    } else if let Some(github) = settings.github.clone() {
        match github_host::connect(&config, &github).await {
            Ok(runtime) => tracker_integration = Some(runtime),
            Err(error) => tracker_error = Some(error),
        }
        Some(TrackerKind::GitHub)
    } else {
        None
    };
    // A machine that will never run a tracker must not tell every client that one
    // is missing, so only a configured tracker is announced. A Linear app that was
    // never installed is worth telling apart from one that will not sign in: they
    // send its owner to different steps. A GitHub App is installed on GitHub, so one
    // that does not answer is one that cannot sign in.
    let readiness = configured.map(|kind| match (&tracker_integration, kind) {
        (Some(_), _) => TrackerReadiness::Ready,
        (None, TrackerKind::Linear) => linear_host::readiness(&credentials_path),
        (None, TrackerKind::GitHub) => TrackerReadiness::Unavailable,
    });
    let trackers = configured
        .zip(readiness)
        .map(|(provider, state)| vec![TrackerConnection { provider, state }])
        .unwrap_or_default();
    match (configured, readiness, &tracker_integration) {
        (Some(kind), Some(_), Some(runtime)) => tracing::info!(
            "{}: {} · webhooks on {}",
            kind.label(),
            runtime.workspace_label(),
            runtime.webhook_address(),
        ),
        (Some(TrackerKind::GitHub), Some(state), None) => tracing::warn!(
            error = tracker_error.as_deref().unwrap_or("no reason given"),
            "GitHub: {} · check [github] in serve.toml and that the app is installed",
            state.said(),
        ),
        (Some(kind), Some(state), None) => tracing::warn!(
            error = tracker_error.as_deref().unwrap_or("no reason given"),
            "{}: {} · run `altitude serve connect {}` on this machine",
            kind.label(),
            state.said(),
            kind.slug()
        ),
        _ => tracing::info!(
            "Task tracker: none configured · run `altitude serve connect linear`, or add a \
             [github] app to serve.toml, to pilot work from a tracker"
        ),
    }

    let identity = InstallationKey::load_or_create(config.data_dir().join("identity.key"))
        .map_err(io::Error::other)?;
    let clients =
        ClientRecord::open(config.data_dir().join("clients.json")).map_err(io::Error::other)?;
    let label = config.identity().map_err(io::Error::other)?.label;
    let isolation = MicrosandboxIsolation::new(&config);
    let prepared = isolation::prepare(isolation.clone());
    // This machine's own answer to "can you be given work", which is not the same
    // question as "is the runtime up": it is ready once its runtime can make
    // sandboxes. Environments are built per operator, after a push or before a
    // run, rather than from a shared configuration at startup.
    let (phase, phases) = watch::channel(Phase::Runtime);
    let database = Database::open(&config).await.map_err(io::Error::other)?;
    let tasks = TaskService::new(
        TaskStore::new(&database),
        isolation,
        config.clone(),
        AgentEngines::new(),
        config.identity().map_err(io::Error::other)?,
    );
    // A remote with no tracker app of its own still reports task status, through
    // the login the task's own peer synced here. When the app is configured,
    // agent sessions own reporting and this would be a second voice.
    let reports_through_pushed_logins = tracker_integration.is_none();
    let mut tasks = tasks.with_issue_reader(
        tracker_integration
            .as_ref()
            .and_then(|integration| integration.issue_reader()),
    );
    if reports_through_pushed_logins {
        tasks = tasks.with_trackers(Arc::new(PushedTrackers::new(config.clone())));
    }
    let (task_loop, mut updates) = TaskEventLoop::waiting_for_runtime(tasks);
    let task_loop = Arc::new(task_loop);
    let watchers = Arc::new(Watchers::default());
    {
        // A machine that finishes preparing says so, rather than leaving the
        // clients that asked before it was ready believing it never got there.
        // What it was doing until then is in its announcement, which a client that
        // is waiting on it reads again; the coarse word is the one that has to
        // reach every watcher, because it is the one that lets them work.
        let watchers = Arc::clone(&watchers);
        let mut phase = phases.clone();
        tokio::spawn(async move {
            while phase.changed().await.is_ok() {
                if phase.borrow_and_update().is_ready() {
                    watchers.ready();
                }
            }
        });
    }
    let logout_sync = Arc::new(Notify::new());
    {
        let watchers = Arc::clone(&watchers);
        let tasks = Arc::clone(&task_loop);
        // A task's history moving is what the tracker has to look at, so it is told
        // here rather than left to ask on a clock of its own.
        let tracker = tracker_integration.clone();
        // The path that reports through the operator's own login has no tracker to
        // wake, so it gets told the same way.
        let sync_signal = Arc::clone(&logout_sync);
        tokio::spawn(async move {
            while let Some(update) = updates.recv().await {
                if let Some(tracker) = &tracker {
                    tracker.wake();
                }
                sync_signal.notify_one();
                match update {
                    // A remote host has no operator in front of it, so a shell opened
                    // in one of its sandboxes is not its to show.
                    TaskExecutionUpdate::ShellOutput { .. }
                    | TaskExecutionUpdate::ShellEnded { .. } => continue,
                    // Bytes for a terminal are not a card: what a watcher reads is
                    // the task, and only the events that carry one change it.
                    TaskExecutionUpdate::Output { .. } => {}
                    TaskExecutionUpdate::Activity { task_id, activity } => {
                        if let Ok(all) = tasks.list().await
                            && let Some(task) = all.iter().find(|task| task.id == task_id)
                            && let Some(operator) = task.operator_id()
                        {
                            let mut summary = TaskSummary::of(task);
                            summary.activity = Some(activity.text);
                            watchers.changed(operator, summary);
                        }
                    }
                    // A handback is the client's business: a remote watching this
                    // task sees the card change when the task moves, not before.
                    TaskExecutionUpdate::HandingBack { .. }
                    | TaskExecutionUpdate::HandedBack(..) => {}
                    TaskExecutionUpdate::Created(Ok(task))
                    | TaskExecutionUpdate::Started(Ok(task))
                    | TaskExecutionUpdate::Finished(_, Ok(task))
                    | TaskExecutionUpdate::OperatorReady(task) => watchers.publish(&task),
                    TaskExecutionUpdate::Removed(id, _) => watchers.removed(id),
                    // A failure that came before a task existed carries no task
                    // with it, so what changed is read from the store rather than
                    // guessed at. Errors are rare; a card left saying the wrong
                    // thing is not.
                    TaskExecutionUpdate::Created(Err(_))
                    | TaskExecutionUpdate::Started(Err(_))
                    | TaskExecutionUpdate::Finished(_, Err(_)) => {
                        if let Ok(all) = tasks.list().await {
                            for task in &all {
                                watchers.publish(task);
                            }
                        }
                    }
                }
            }
        });
    }
    if reports_through_pushed_logins {
        let syncing = Arc::clone(&task_loop);
        tokio::spawn(async move {
            // What a task's history added is what has to be reported, so the queue is
            // drained when it moves rather than every ten seconds whether it did.
            loop {
                logout_sync.notified().await;
                if let Err(error) = syncing.service().sync_tracker().await {
                    tracing::warn!(%error, "tracker updates remain queued");
                }
            }
        });
    }
    let incoming = IncomingTransfers::new(config.clone(), task_loop.clone());
    let host = Arc::new(OwnTasks {
        config: Arc::new(config.clone()),
        tasks: task_loop,
        incoming,
        preparation_lock: Arc::new(AsyncMutex::new(())),
        transfers: Arc::new(Transfers::new()),
        collecting: Arc::new(Mutex::new(HashMap::new())),
        watchers,
        read_slots: Arc::new(Semaphore::new(4)),
        readiness: phase.clone(),
        runtime: prepared,
        tracker: tracker_integration.clone(),
        delegation_paused: settings.pause_delegated_work,
    });
    let mut waking = None;
    if let Some(integration) = tracker_integration {
        waking = Some(Arc::clone(&integration));
        let delegated_work: Arc<dyn DelegatedWork> = host.clone();
        integration.run(delegated_work);
    }
    let server = LinkServer::serve(ServerParts {
        identity,
        clients,
        listen,
        label,
        data_dir: config.data_dir().to_owned(),
        phase: phases,
        trackers,
        tasks: host.clone(),
    })
    .await
    .map_err(io::Error::other)?;
    let ticket = server.ticket().map_err(io::Error::other)?;
    let connect_address = if listen.ip().is_unspecified() {
        whoami::hostname()
            .ok()
            .filter(|host| !host.is_empty())
            .map(|host| {
                if listen.port() == DEFAULT_PORT {
                    host
                } else {
                    format!("{host}:{}", listen.port())
                }
            })
    } else {
        Some(listen.to_string())
    };

    tracing::info!(address = %listen, "Listening · TCP discovery and UDP task connections · preparing sandboxes");
    tracing::debug!(identity = %server.id(), %ticket, "Remote connection details");

    // Readiness is behind the server, so the answer to "what can you do" is
    // available while the answer is still "not yet".
    let mut runtime = host.runtime.clone();
    let readiness = host.readiness.clone();
    let resumed = host.tasks.clone();
    let preparation = async {
        loop {
            let status = runtime.borrow_and_update().clone();
            let said = match status {
                IsolationStatus::Starting => Phase::Runtime,
                IsolationStatus::InstallingRuntime => {
                    tracing::info!("Installing sandbox runtime · first startup may take a moment");
                    Phase::Runtime
                }
                IsolationStatus::PreparingBaseImage(pull) => {
                    tracing::info!("Preparing base sandbox image");
                    Phase::BaseImage {
                        downloaded_bytes: pull.downloaded_bytes,
                        total_bytes: pull.total_bytes,
                    }
                }
                IsolationStatus::PreparingBasePackages(install) => {
                    tracing::info!(
                        installed = install.installed,
                        total = install.total,
                        "Installing base tools"
                    );
                    Phase::BaseTools
                }
                IsolationStatus::Failed(error) => return Err(io::Error::other(error)),
                IsolationStatus::Ready => break,
            };
            phase.send_replace(said);
            runtime.changed().await.map_err(io::Error::other)?;
        }
        tracing::info!("Sandbox ready · recovering saved tasks");
        for result in resumed.service().recover_all().await {
            if let Err(error) = result {
                tracing::warn!(%error, "a task needs attention after restart");
            }
        }
        // Recovery is the one place a task's history moves without a run to say so,
        // and the tracker wakes on a run's updates: without this, what a restart
        // concluded sits in the history until something else happens to arrive, and
        // a task waiting on the operator says so only after they have written again.
        if let Some(tracker) = waking {
            tracker.wake();
        }
        resumed.enable_admission();
        readiness.send_replace(Phase::Ready);
        tracing::info!("Remote ready · open altitude → Settings → Remotes");
        if !listen.ip().is_loopback() {
            tracing::info!("Clients on the same network can discover this remote automatically");
        }
        if let Some(address) = &connect_address {
            tracing::info!(address = %address, "To add it manually, enter this address");
        } else {
            tracing::info!("To add it manually, enter this machine’s hostname");
        }
        Ok::<(), io::Error>(())
    };
    tokio::pin!(preparation);

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    // A transfer nobody is reading is a task nobody is running, and the only thing
    // that can tell is the machine holding it. It knows the moment a transfer in
    // flight would be stale, so that is what this waits for: no transfer, no wait.
    let mut transfers = host.watch_transfers();
    let mut is_ready = false;
    let result = loop {
        let stale_at = *transfers.borrow_and_update();
        tokio::select! {
            _ = time::sleep_until(stale_at.unwrap_or_else(Instant::now).into()), if stale_at.is_some() => {
                host.release_stalled_transfers().await;
            }
            _ = transfers.changed() => {}
            result = &mut preparation, if !is_ready => match result {
                Ok(()) => is_ready = true,
                Err(error) => {
                    tracing::error!(%error, logs = %config.isolation_data_dir().join("sandboxes/altitude-base-image-setup/logs").display(), "Remote startup failed · no tasks will be accepted");
                    break Err(error);
                }
            },
            _ = terminate.recv() => break Ok(()),
            _ = interrupt.recv() => break Ok(()),
        }
    };

    tracing::info!("Stopping remote · suspending active tasks");
    // Whoever is watching a delegated task sees the session stop, so it is told why
    // before the process goes: a session that just ends reads as a hang. It goes
    // first and it is bounded, because the tasks are what the minute is for.
    if let Some(tracker) = host.tracker.as_ref() {
        tracker
            .announce_shutdown(host.as_ref() as &dyn DelegatedWork)
            .await;
    }
    // Suspension comes before the links close: every task's disk is flushed to
    // this host first, so a link that refuses to shut down cleanly is a smaller
    // loss than work that never reached a snapshot. The budget is what keeps the
    // wind-down ours: a host that is going away gives a guest a minute, and being
    // cut off inside a snapshot is worse than a task that starts again afterwards.
    let suspended = match timeout(STOP_BUDGET, host.tasks.suspend_all()).await {
        Ok(suspended) => suspended,
        Err(_) => {
            tracing::warn!(
                budget_s = STOP_BUDGET.as_secs(),
                "stopping took longer than the wind-down has; tasks that were not parked are picked up after the restart"
            );
            Ok(())
        }
    };
    let stopped = server.shutdown().await.map_err(io::Error::other);
    result?;
    suspended?;
    stopped?;
    tracing::info!("Remote stopped");
    Ok(())
}

pub(super) async fn wait_until_ready(
    runtime: &mut watch::Receiver<IsolationStatus>,
) -> Result<(), String> {
    loop {
        match &*runtime.borrow_and_update() {
            IsolationStatus::Ready => return Ok(()),
            IsolationStatus::Failed(error) => return Err(error.clone()),
            _ => {}
        }
        runtime.changed().await.map_err(|e| e.to_string())?;
    }
}

/// Build the environment each project of a pushed configuration runs in. A first
/// push builds them from nothing, so each one is what the machine says it is doing
/// while it lasts.
pub(super) async fn prepare_projects(
    tasks: &TaskEventLoop<MicrosandboxIsolation>,
    owner: &str,
    projects: &[Project],
    agent: AgentKind,
    phase: &watch::Sender<Phase>,
) -> Result<(), String> {
    for project in projects {
        phase.send_replace(Phase::Project {
            name: project.name.clone(),
        });
        tasks
            .prepare_project_as_operator(owner, project, agent, &Unwatched)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
