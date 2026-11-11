use std::{
    collections::HashMap,
    error::Error,
    fmt, fs, io,
    io::IsTerminal,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tempfile::{NamedTempFile, TempDir};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

use iroh::{
    EndpointAddr,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use tokio::{
    fs::File as AsyncFile,
    io::copy,
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    task::JoinHandle,
    time,
};

use crate::{
    agent::{AgentEngines, mcp},
    brand,
    cli::{WORKER_GUIDE, WorkerArgs},
    config::Config,
    credentials::profile_recipe,
    isolation::{
        CommandEvent, CredentialSandbox, IsolationEngine, IsolationStatus, ProfileRecipe,
        TestbedIsolation, Unwatched, UnwatchedFiles, VsockRoute, mounts, prepare,
        private_directory,
        worker_protocol::{
            CommandFrame, WORKER_ALPN, WorkerCall, WorkerProfile, WorkerReply, WorkerRequest,
        },
    },
    logging,
    remote::{
        InstallationKey, LinkClient,
        protocol::{RemoteTicket, WorkerReport, builder, read_frame, write_frame},
    },
    version::VERSION,
};

use super::workers::HEARTBEAT_INTERVAL;

// The other end of the fleet contract: an empty host that knows nothing but who
// made it. It registers, says it is still there, and answers the master's calls
// for sandboxes on the same endpoint it registered over.
pub(crate) async fn run(args: &WorkerArgs, base: Config) -> Result<(), Box<dyn Error>> {
    // A worker is spawned by a script, never typed at a terminal. Running one by
    // hand is how a host becomes a host by accident, so say what this is instead
    // of taking the machine over.
    if !args.yes && io::stdin().is_terminal() && io::stdout().is_terminal() {
        println!("{WORKER_GUIDE}");
        return Ok(());
    }
    logging::init_service();
    tracing::info!(
        "{}  altitude {VERSION} · isolation worker {}",
        brand::MOUNTAIN,
        args.name
    );
    let key = InstallationKey::load_or_create(base.data_dir().join("worker.key"))
        .map_err(io::Error::other)?;
    // A host is spawned for one task and its whole host is the sandbox, so it
    // needs no nested microVM: the host is the boundary. The host engine is the
    // closest thing we have — a chroot over the machine, not a microVM.
    let engine = TestbedIsolation::new(&base);
    // Preparing pulls the runtime and builds the base image, which is minutes of
    // work. The host registers before it is ready so the master can see it, and
    // says it is not ready until this finishes.
    let readiness = Arc::new(Mutex::new(Readiness::Preparing));
    {
        let mut status = prepare(engine.clone());
        let readiness = Arc::clone(&readiness);
        tokio::spawn(async move {
            loop {
                match &*status.borrow_and_update() {
                    IsolationStatus::Ready => {
                        tracing::info!("sandbox runtime ready");
                        *readiness.lock().expect("worker readiness") = Readiness::Ready;
                        return;
                    }
                    IsolationStatus::Failed(error) => {
                        tracing::error!(%error, "the sandbox runtime could not be prepared");
                        *readiness.lock().expect("worker readiness") =
                            Readiness::Failed(error.clone());
                        return;
                    }
                    _ => {}
                }
                if status.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    let endpoint = builder()
        .secret_key(key.secret_key().clone())
        .alpns(vec![WORKER_ALPN.to_vec()])
        .bind()
        .await
        .map_err(io::Error::other)?;
    let client = LinkClient::from_endpoint(endpoint.clone());
    let master: RemoteTicket = args.master.parse().map_err(io::Error::other)?;
    let peer: EndpointAddr = master.into();
    let service = WorkerService {
        engine,
        config: base.clone(),
        agents: AgentEngines::new(),
        token: args.token.clone(),
        snapshots_dir: base.task_snapshots_dir(),
        readiness,
        client: client.clone(),
        master: peer.clone(),
        channels: Arc::new(Mutex::new(HashMap::new())),
    };
    let _router = Router::builder(endpoint)
        .accept(WORKER_ALPN, service)
        .spawn();

    let capacity = local_capacity();
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;

    'serving: loop {
        loop {
            match client
                .register_worker(peer.clone(), &args.token, &args.name, capacity)
                .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::warn!(%error, "could not register with the master; retrying");
                    tokio::select! {
                        _ = time::sleep(Duration::from_secs(5)) => {}
                        _ = terminate.recv() => break 'serving,
                        _ = interrupt.recv() => break 'serving,
                    }
                }
            }
        }
        tracing::info!(master = %args.master, "registered as an isolation worker");
        loop {
            tokio::select! {
                _ = time::sleep(HEARTBEAT_INTERVAL) => {
                    if let Err(error) = client.worker_heartbeat(peer.clone(), &args.name).await {
                        tracing::warn!(%error, "a heartbeat did not reach the master; registering again");
                        break;
                    }
                }
                _ = terminate.recv() => break 'serving,
                _ = interrupt.recv() => break 'serving,
            }
        }
    }
    client.shutdown().await;
    tracing::info!("isolation worker stopped");
    Ok(())
}

struct WorkerService {
    engine: TestbedIsolation,
    config: Config,
    // The same provider definitions the master builds recipes from, so a profile
    // id is enough to rebuild a recipe here.
    agents: AgentEngines,
    token: String,
    snapshots_dir: PathBuf,
    readiness: Arc<Mutex<Readiness>>,
    client: LinkClient,
    master: EndpointAddr,
    // The loopback listeners carrying each sandbox's agent channel to the master.
    channels: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

// Whether this host can make sandboxes yet. Preparing is minutes, and a master
// that asks before it is done is told to wait rather than given a failure.
enum Readiness {
    Preparing,
    Ready,
    Failed(String),
}

impl fmt::Debug for WorkerService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkerService")
    }
}

impl ProtocolHandler for WorkerService {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let Ok((send, recv)) = connection.accept_bi().await else {
            return Ok(());
        };
        let mut send = send;
        let mut recv = recv;
        let request: WorkerRequest = match read_frame(&mut recv).await {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(%error, "an unreadable worker call arrived");
                return Ok(());
            }
        };
        if request.token != self.token {
            let _ = write_frame(
                &mut send,
                &WorkerReply::Failed("this host does not hold the master's token".into()),
            )
            .await;
            return Ok(());
        }
        match request.call {
            WorkerCall::RunCommand { id, command } => {
                self.run_command(send, recv, id, command).await;
                Ok(())
            }
            WorkerCall::DeliverFile { id, guest_path } => {
                self.receive_file(send, recv, id, guest_path).await;
                Ok(())
            }
            WorkerCall::DeliverFiles { id, root } => {
                self.receive_tree(send, recv, id, root).await;
                Ok(())
            }
            WorkerCall::CollectFile { id, guest_path } => {
                self.send_file(send, recv, id, guest_path).await;
                Ok(())
            }
            WorkerCall::CollectFiles { id, root, excluded } => {
                self.send_tree(send, recv, id, root, excluded).await;
                Ok(())
            }
            call => {
                let reply = self.dispatch(call).await;
                let _ = write_frame(&mut send, &reply).await;
                let _ = send.finish();
                Ok(())
            }
        }
    }
}

impl WorkerService {
    // The host's end of the agent channel: it listens on the loopback the sandbox
    // shares and carries each connection to the master over the link. A vsock route
    // for the agent's channel is what says a sandbox has one.
    fn start_channels(&self, id: &str, vsock: &[VsockRoute]) {
        for route in vsock {
            if route.guest_port != mcp::AGENT_CHANNEL_PORT {
                continue;
            }
            let socket = route.host_socket.to_string_lossy().into_owned();
            let port = route.guest_port as u16;
            let client = self.client.clone();
            let master = self.master.clone();
            let token = self.token.clone();
            let handle = tokio::spawn(async move {
                let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        tracing::warn!(%error, port, "could not open an agent channel on this host");
                        return;
                    }
                };
                loop {
                    match listener.accept().await {
                        Ok((tcp, _)) => {
                            let client = client.clone();
                            let master = master.clone();
                            let token = token.clone();
                            let socket = socket.clone();
                            tokio::spawn(async move {
                                if let Err(error) = client
                                    .pipe_agent_channel(master, &socket, &token, tcp)
                                    .await
                                {
                                    tracing::debug!(%error, "an agent channel connection ended");
                                }
                            });
                        }
                        Err(error) => {
                            tracing::warn!(%error, "an agent channel listener stopped");
                            return;
                        }
                    }
                }
            });
            if let Ok(mut channels) = self.channels.lock() {
                channels.insert(id.to_owned(), handle);
            }
        }
    }

    fn stop_channel(&self, id: &str) {
        if let Ok(mut channels) = self.channels.lock()
            && let Some(handle) = channels.remove(id)
        {
            handle.abort();
        }
    }

    // The bytes on the stream are a file the master is putting in the sandbox.
    async fn receive_file(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
        id: String,
        guest_path: String,
    ) {
        let reply = match receive_to_temp(&mut recv).await {
            Ok(temp) => {
                match self
                    .engine
                    .deliver_file(id, temp.path().to_path_buf(), guest_path)
                    .await
                {
                    Ok(()) => WorkerReply::Ok,
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            Err(error) => WorkerReply::Failed(error),
        };
        let _ = write_frame(&mut send, &reply).await;
        let _ = send.finish();
    }

    // The bytes on the stream are a tar of a tree the master is putting in.
    async fn receive_tree(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
        id: String,
        root: String,
    ) {
        let reply = match receive_to_temp(&mut recv).await {
            Ok(tar) => match TempDir::new() {
                Ok(directory) => match extract_tree(tar.path(), directory.path()) {
                    Ok(()) => match self
                        .engine
                        .deliver_files(id, directory.path().to_path_buf(), root, &UnwatchedFiles)
                        .await
                    {
                        Ok(_) => WorkerReply::Ok,
                        Err(error) => WorkerReply::Failed(error.to_string()),
                    },
                    Err(error) => WorkerReply::Failed(error),
                },
                Err(error) => WorkerReply::Failed(error.to_string()),
            },
            Err(error) => WorkerReply::Failed(error),
        };
        let _ = write_frame(&mut send, &reply).await;
        let _ = send.finish();
    }

    async fn send_file(
        &self,
        mut send: SendStream,
        _recv: RecvStream,
        id: String,
        guest_path: String,
    ) {
        if let Ok(temp) = NamedTempFile::new()
            && let Ok(len) = self
                .engine
                .collect_file(id, guest_path, temp.path().to_path_buf())
                .await
        {
            if write_frame(
                &mut send,
                &WorkerReply::Blob {
                    len,
                    files: 1,
                    bytes: len,
                },
            )
            .await
            .is_ok()
                && let Ok(mut file) = AsyncFile::open(temp.path()).await
            {
                let _ = copy(&mut file, &mut send).await;
            }
            let _ = send.finish();
            return;
        }
        let _ = write_frame(
            &mut send,
            &WorkerReply::Failed("could not collect that file".into()),
        )
        .await;
        let _ = send.finish();
    }

    async fn send_tree(
        &self,
        mut send: SendStream,
        _recv: RecvStream,
        id: String,
        root: String,
        excluded: Vec<String>,
    ) {
        if let Ok(directory) = TempDir::new()
            && let Ok(tally) = self
                .engine
                .collect_files(
                    id,
                    root,
                    excluded,
                    directory.path().to_path_buf(),
                    &UnwatchedFiles,
                )
                .await
            && let Ok(tar) = NamedTempFile::new()
            && let Ok(len) = pack_tree(directory.path(), tar.path())
        {
            if write_frame(
                &mut send,
                &WorkerReply::Blob {
                    len,
                    files: tally.files,
                    bytes: tally.bytes,
                },
            )
            .await
            .is_ok()
                && let Ok(mut file) = AsyncFile::open(tar.path()).await
            {
                let _ = copy(&mut file, &mut send).await;
            }
            let _ = send.finish();
            return;
        }
        let _ = write_frame(
            &mut send,
            &WorkerReply::Failed("could not collect that tree".into()),
        )
        .await;
        let _ = send.finish();
    }

    async fn dispatch(&self, call: WorkerCall) -> WorkerReply {
        match call {
            WorkerCall::Probe => {
                let readiness = self.readiness.lock().expect("worker readiness");
                match &*readiness {
                    Readiness::Ready => WorkerReply::Ready,
                    Readiness::Preparing => WorkerReply::Failed(
                        "this host is still preparing its sandbox runtime".into(),
                    ),
                    Readiness::Failed(error) => WorkerReply::Failed(format!(
                        "this host cannot prepare its sandbox runtime: {error}"
                    )),
                }
            }
            WorkerCall::EnsureProject { project, agent } => {
                let recipes = mounts::task(&self.agents, agent);
                match self
                    .engine
                    .ensure_project(project, recipes, &Unwatched)
                    .await
                {
                    Ok(snapshot) => WorkerReply::Snapshot(snapshot),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::Provision {
                id,
                snapshot,
                project,
                agent_profile,
                vcs_profile,
                vsock,
                profiles,
            } => {
                if let Err(error) = write_profiles(&self.config, &profiles) {
                    return WorkerReply::Failed(error);
                }
                let recipes = match (
                    profile_recipe(&agent_profile, &self.agents),
                    profile_recipe(&vcs_profile, &self.agents),
                ) {
                    (Ok(agent), Ok(vcs)) => Ok(vec![agent, vcs]),
                    (Err(error), _) | (_, Err(error)) => Err(error),
                };
                match recipes {
                    Ok(recipes) => {
                        let channels = vsock.clone();
                        match self
                            .engine
                            .provision(id.clone(), snapshot, project, recipes, vsock, &Unwatched)
                            .await
                        {
                            Ok(()) => {
                                self.start_channels(&id, &channels);
                                WorkerReply::Ok
                            }
                            Err(error) => WorkerReply::Failed(error.to_string()),
                        }
                    }
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::Create {
                id,
                options,
                purpose,
            } => match self.engine.create(id, options, purpose).await {
                Ok(()) => WorkerReply::Ok,
                Err(error) => WorkerReply::Failed(error.to_string()),
            },
            WorkerCall::Remove { id } => {
                self.stop_channel(&id);
                match self.engine.remove(id).await {
                    Ok(()) => WorkerReply::Ok,
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::HasSandbox { id } => WorkerReply::Bool(self.engine.has_sandbox(id).await),
            WorkerCall::Suspend { id, name } => {
                self.stop_channel(&id);
                match self
                    .engine
                    .suspend(id, name, self.snapshots_dir.clone())
                    .await
                {
                    Ok(path) => WorkerReply::Path(path),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::RemoveSnapshot { path } => match self.engine.remove_snapshot(path).await {
                Ok(()) => WorkerReply::Ok,
                Err(error) => WorkerReply::Failed(error.to_string()),
            },
            WorkerCall::GuestFileExists { id, path } => {
                match self.engine.guest_regular_file_exists(id, path).await {
                    Ok(exists) => WorkerReply::Bool(exists),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::ProjectMemoryDir { project_id } => WorkerReply::Text(
                self.engine
                    .project_memory_dir(&project_id)
                    .to_string_lossy()
                    .into_owned(),
            ),
            WorkerCall::BaseDescription => WorkerReply::Description(self.engine.base_description()),
            WorkerCall::Pressure => WorkerReply::Pressure(self.engine.pressure()),
            WorkerCall::Room => WorkerReply::Room(self.engine.sandbox_room()),
            WorkerCall::CleanupLeftovers => {
                self.engine.remove_leftover_sandboxes().await;
                WorkerReply::Ok
            }
            WorkerCall::CredentialShell {
                id,
                profiles,
                snapshot,
                vsock,
                profile,
            } => {
                if let Err(error) = write_profiles(&self.config, &profiles) {
                    return WorkerReply::Failed(error);
                }
                let recipes = match rebuild_recipes(&profiles, &self.agents) {
                    Ok(recipes) => recipes,
                    Err(error) => return WorkerReply::Failed(error),
                };
                let sandbox = CredentialSandbox {
                    id,
                    recipes,
                    snapshot,
                    vsock,
                };
                match self
                    .engine
                    .credential_shell(sandbox, &profile, &Unwatched)
                    .await
                {
                    Ok(command) => WorkerReply::Text(command),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::Watcher {
                id,
                profiles,
                project,
            } => {
                if let Err(error) = write_profiles(&self.config, &profiles) {
                    return WorkerReply::Failed(error);
                }
                let recipes = match rebuild_recipes(&profiles, &self.agents) {
                    Ok(recipes) => recipes,
                    Err(error) => return WorkerReply::Failed(error),
                };
                match self.engine.watcher(id, project, recipes, &Unwatched).await {
                    Ok(()) => WorkerReply::Ok,
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::CredentialReports {
                id,
                profiles,
                snapshot,
                vsock,
                checks,
            } => {
                if let Err(error) = write_profiles(&self.config, &profiles) {
                    return WorkerReply::Failed(error);
                }
                let recipes = match rebuild_recipes(&profiles, &self.agents) {
                    Ok(recipes) => recipes,
                    Err(error) => return WorkerReply::Failed(error),
                };
                let sandbox = CredentialSandbox {
                    id,
                    recipes,
                    snapshot,
                    vsock,
                };
                match self
                    .engine
                    .credential_reports(sandbox, checks, &Unwatched)
                    .await
                {
                    Ok(reports) => WorkerReply::Reports(reports),
                    Err(error) => WorkerReply::Failed(error.to_string()),
                }
            }
            WorkerCall::RunCommand { .. }
            | WorkerCall::DeliverFile { .. }
            | WorkerCall::DeliverFiles { .. }
            | WorkerCall::CollectFile { .. }
            | WorkerCall::CollectFiles { .. } => unreachable!("this call is streamed"),
        }
    }

    async fn run_command(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
        id: String,
        command: String,
    ) {
        let mut session = match self.engine.run_command(id, command).await {
            Ok(session) => session,
            Err(error) => {
                let _ = write_frame(&mut send, &WorkerReply::Failed(error.to_string())).await;
                return;
            }
        };
        if write_frame(&mut send, &WorkerReply::Ready).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                event = session.events.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    let done = matches!(event, CommandEvent::Exited(_) | CommandEvent::Failed(_));
                    if write_frame(&mut send, &CommandFrame::Report(event)).await.is_err() {
                        return;
                    }
                    if done {
                        break;
                    }
                }
                frame = read_frame::<_, CommandFrame>(&mut recv) => {
                    match frame {
                        Ok(CommandFrame::Drive(command)) => {
                            if session.commands.send(command).is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
        let _ = send.finish();
    }
}

fn rebuild_recipes(
    profiles: &[WorkerProfile],
    agents: &AgentEngines,
) -> Result<Vec<ProfileRecipe>, String> {
    profiles
        .iter()
        .map(|profile| profile_recipe(&profile.profile, agents).map_err(|error| error.to_string()))
        .collect()
}

// The profile files the master pushed with this sandbox, written where the plan
// will mount the profile. A file's path is relative to the profile's directory and
// may not climb out of it.
fn write_profiles(config: &Config, profiles: &[WorkerProfile]) -> Result<(), String> {
    for profile in profiles {
        let root = config.profile_dir(&profile.profile);
        private_directory(&root).map_err(|error| error.to_string())?;
        for (path, contents) in &profile.files {
            let target = root.join(path);
            if !target.starts_with(&root) {
                return Err(format!("a profile path leaves its directory: {path}"));
            }
            let bytes = BASE64
                .decode(contents)
                .map_err(|error| format!("an unreadable profile file: {error}"))?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::write(&target, bytes).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

// The bytes on an incoming stream, kept in a temporary file the engine can read as
// a host path.
async fn receive_to_temp(recv: &mut RecvStream) -> Result<NamedTempFile, String> {
    let temp = NamedTempFile::new().map_err(|error| error.to_string())?;
    let mut file = AsyncFile::create(temp.path())
        .await
        .map_err(|error| error.to_string())?;
    copy(recv, &mut file)
        .await
        .map_err(|error| error.to_string())?;
    file.sync_all().await.map_err(|error| error.to_string())?;
    Ok(temp)
}

fn pack_tree(source: &Path, tar_path: &Path) -> Result<u64, String> {
    let file = fs::File::create(tar_path).map_err(|error| error.to_string())?;
    let mut builder = tar::Builder::new(file);
    builder
        .append_dir_all(".", source)
        .map_err(|error| error.to_string())?;
    builder.finish().map_err(|error| error.to_string())?;
    fs::metadata(tar_path)
        .map(|metadata| metadata.len())
        .map_err(|error| error.to_string())
}

fn extract_tree(tar_path: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let file = fs::File::open(tar_path).map_err(|error| error.to_string())?;
    let mut archive = tar::Archive::new(file);
    archive
        .unpack(destination)
        .map_err(|error| error.to_string())
}

fn local_capacity() -> WorkerReport {
    let Ok(mut system) = sysinfo::System::new() else {
        return WorkerReport {
            cpus: 1,
            memory_mib: 0,
        };
    };
    system.refresh_memory();
    WorkerReport {
        cpus: system.cpus().len().max(1),
        memory_mib: system.total_memory() / (1024 * 1024),
    }
}
