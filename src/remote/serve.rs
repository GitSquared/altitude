//! The serve role: work for clients, with no terminal.
//!
//! A different execution mode, not a different configuration of the same one. A
//! serve role has nobody at the keyboard, so it never takes the terminal, it logs to
//! stdout for whatever service manager started it, and it reads its own settings
//! file rather than the client's.

use std::{env, error::Error, fs, io, net::SocketAddr};

use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use std::sync::{Arc, Mutex};

use microsandbox::{SaveOpts, Snapshot};

use super::{CapsuleArchive, Handover, HandoverDraft, RecordedEvent};

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
    /// Capsules already bundled for travel, and where their archives are.
    ///
    /// Kept until the destination confirms. A transfer nobody confirmed leaves the
    /// task where it was rather than nowhere, which is the failure that repairs
    /// itself.
    handed: Arc<Mutex<HashMap<String, Handover>>>,
    archives: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl TaskHost for OwnTasks {
    fn hand_over(&self, task_id: String) -> BoxFuture<Result<Handover, String>> {
        let tasks = Arc::clone(&self.tasks);
        let handed = Arc::clone(&self.handed);
        let archives = Arc::clone(&self.archives);
        Box::pin(async move {
            let owned = tasks.list().await.map_err(|error| error.to_string())?;
            let task = owned
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| format!("this machine does not own {task_id}"))?;
            // The source stops running a task before any of it moves, and stopping is
            // also what settles the capsule: what travels is the disk as it is now.
            tasks
                .suspend(task.clone())
                .await
                .map_err(|error| error.to_string())?;
            let stopped = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| format!("{task_id} went missing while it was stopping"))?;
            let events = tasks
                .events(&stopped.id)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|(recorded_at_ms, event)| RecordedEvent {
                    recorded_at_ms,
                    event,
                })
                .collect();
            let (capsule, archive) = bundle(&stopped).await?;
            let handover = Handover {
                task_id: stopped.id.clone(),
                draft: HandoverDraft {
                    intent: stopped.facts.intent.clone(),
                    environment: stopped.facts.environment_name.clone(),
                    agent: stopped.agent_kind.unwrap_or_default(),
                    agent_conversation_id: stopped.facts.agent_conversation_id.clone(),
                },
                events,
                capsule,
            };
            handed
                .lock()
                .map_err(|_| "the transfers were poisoned".to_owned())?
                .insert(stopped.id.clone(), handover.clone());
            archives
                .lock()
                .map_err(|_| "the transfers were poisoned".to_owned())?
                .insert(stopped.id, archive);
            Ok(handover)
        })
    }

    fn capsule_piece(
        &self,
        task_id: String,
        offset: u64,
        len: u64,
    ) -> BoxFuture<Result<Vec<u8>, String>> {
        let archives = Arc::clone(&self.archives);
        Box::pin(async move {
            let path = archives
                .lock()
                .map_err(|_| "the transfers were poisoned".to_owned())?
                .get(&task_id)
                .cloned()
                .ok_or_else(|| format!("{task_id} has not been handed over"))?;
            let mut file = fs::File::open(&path).map_err(|error| error.to_string())?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| error.to_string())?;
            let mut bytes = vec![0; usize::try_from(len).unwrap_or(usize::MAX)];
            let read = file.read(&mut bytes).map_err(|error| error.to_string())?;
            bytes.truncate(read);
            Ok(bytes)
        })
    }

    fn handed_over(&self, task_id: String) -> BoxFuture<Result<(), String>> {
        let tasks = Arc::clone(&self.tasks);
        let handed = Arc::clone(&self.handed);
        let archives = Arc::clone(&self.archives);
        Box::pin(async move {
            let owned = tasks.list().await.map_err(|error| error.to_string())?;
            let task = owned
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| format!("this machine does not own {task_id}"))?;
            // The destination holds all of it, so this machine is not the owner any
            // more. The record stays, because what was decided here is worth keeping
            // even though the task has moved on.
            tasks.remove(task);
            handed
                .lock()
                .map_err(|_| "the transfers were poisoned".to_owned())?
                .remove(&task_id);
            let archive = archives
                .lock()
                .map_err(|_| "the transfers were poisoned".to_owned())?
                .remove(&task_id);
            if let Some(archive) = archive {
                let _ = fs::remove_file(archive);
            }
            Ok(())
        })
    }

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

/// Bundle a task's capsule for travel, and say where the archive is.
///
/// Microsandbox does the bundling: it writes a `.tar.zst` that understands sparse
/// files, so a 15 GB disk whose filesystem has barely been written to costs what has
/// actually been written and not what it could hold. Nothing here knows what a
/// capsule is made of, which is the point.
async fn bundle(task: &Task) -> Result<(CapsuleArchive, PathBuf), String> {
    let Some(path) = task.facts.snapshot_path.as_ref() else {
        return Ok((
            CapsuleArchive {
                len: 0,
                digest: String::new(),
            },
            PathBuf::new(),
        ));
    };
    let directory = env::temp_dir().join("altitude-transfers");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let archive = directory.join(format!("{}.tar.zst", task.id));
    let snapshot = Snapshot::open(path)
        .await
        .map_err(|error| error.to_string())?;
    let since = base_every_machine_has(&snapshot).await;
    snapshot
        .save_to(
            &archive,
            SaveOpts {
                since,
                ..SaveOpts::default()
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let len = fs::metadata(&archive)
        .map_err(|error| error.to_string())?
        .len();
    Ok((
        CapsuleArchive {
            len,
            digest: digest_of(&archive)?,
        },
        archive,
    ))
}

/// The base of a capsule, when that base is one every machine built for itself.
///
/// A task's chain starts at the environment it was built on, and every machine in a
/// replica builds that environment from the same configuration. Omitting it is the
/// difference between moving a task and moving a machine.
///
/// A task that has run and been resumed before has its own earlier capsules in the
/// chain as well, and those are not something another machine can look up. So this
/// answers only where the chain is the environment and nothing else, and leaves the
/// archive whole otherwise: an archive missing a layer nobody has is not an archive.
async fn base_every_machine_has(snapshot: &Snapshot) -> Option<String> {
    let parent = snapshot
        .manifest()
        .parent
        .as_ref()
        .map(ToString::to_string)?;
    let ancestor = Snapshot::open(&parent).await.ok()?;
    match ancestor.manifest().parent {
        Some(_) => None,
        None => Some(parent),
    }
}

/// Hash a file without holding it in memory, which an archive can be too large for.
fn digest_of(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
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
            handed: Arc::new(Mutex::new(HashMap::new())),
            archives: Arc::new(Mutex::new(HashMap::new())),
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
