use std::{
    error::Error,
    fmt, io,
    io::IsTerminal,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use iroh::{
    EndpointAddr,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use tokio::{
    signal::unix::{SignalKind, signal},
    time,
};

use crate::{
    brand,
    cli::{WORKER_GUIDE, WorkerArgs},
    config::Config,
    isolation::{
        CommandEvent, IsolationEngine, IsolationStatus, MicrosandboxIsolation, prepare,
        worker_protocol::{CommandFrame, WORKER_ALPN, WorkerCall, WorkerReply, WorkerRequest},
    },
    logging,
    remote::{
        InstallationKey, LinkClient,
        protocol::{RemoteTicket, WorkerReport, builder, read_frame, write_frame},
    },
    version::VERSION,
};

use super::workers::HEARTBEAT_INTERVAL;

// The other end of the fleet contract: an empty box that knows nothing but who
// made it. It registers, says it is still there, and answers the master's calls
// for sandboxes on the same endpoint it registered over.
pub(crate) async fn run(args: &WorkerArgs, base: Config) -> Result<(), Box<dyn Error>> {
    // A worker is spawned by a script, never typed at a terminal. Running one by
    // hand is how a host becomes a box by accident, so say what this is instead
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
    // A box that cannot make sandboxes is not a box, so it says so before it
    // registers rather than taking work it will fail.
    MicrosandboxIsolation::check_host().map_err(io::Error::other)?;
    let engine = MicrosandboxIsolation::new(&base);
    // Preparing pulls the runtime and builds the base image, which is minutes of
    // work. The box registers before it is ready so the master can see it, and
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
    let service = WorkerService {
        engine,
        token: args.token.clone(),
        snapshots_dir: base.task_snapshots_dir(),
        readiness,
    };
    let _router = Router::builder(endpoint)
        .accept(WORKER_ALPN, service)
        .spawn();

    let master: RemoteTicket = args.master.parse().map_err(io::Error::other)?;
    let peer: EndpointAddr = master.into();
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
    engine: MicrosandboxIsolation,
    token: String,
    snapshots_dir: PathBuf,
    readiness: Arc<Mutex<Readiness>>,
}

// Whether this box can make sandboxes yet. Preparing is minutes, and a master
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
                &WorkerReply::Failed("this box does not hold the master's token".into()),
            )
            .await;
            return Ok(());
        }
        if let WorkerCall::RunCommand { id, command } = request.call {
            self.run_command(send, recv, id, command).await;
            return Ok(());
        }
        let reply = self.dispatch(request.call).await;
        let _ = write_frame(&mut send, &reply).await;
        let _ = send.finish();
        Ok(())
    }
}

impl WorkerService {
    async fn dispatch(&self, call: WorkerCall) -> WorkerReply {
        match call {
            WorkerCall::Probe => {
                let readiness = self.readiness.lock().expect("worker readiness");
                match &*readiness {
                    Readiness::Ready => WorkerReply::Ready,
                    Readiness::Preparing => WorkerReply::Failed(
                        "this box is still preparing its sandbox runtime".into(),
                    ),
                    Readiness::Failed(error) => WorkerReply::Failed(format!(
                        "this box cannot prepare its sandbox runtime: {error}"
                    )),
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
            WorkerCall::Remove { id } => match self.engine.remove(id).await {
                Ok(()) => WorkerReply::Ok,
                Err(error) => WorkerReply::Failed(error.to_string()),
            },
            WorkerCall::HasSandbox { id } => WorkerReply::Bool(self.engine.has_sandbox(id).await),
            WorkerCall::Suspend { id, name } => {
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
            WorkerCall::RunCommand { .. } => unreachable!("commands are streamed"),
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
