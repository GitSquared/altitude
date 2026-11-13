use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    io::IsTerminal,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(target_os = "linux")]
use fuser::BackgroundSession;

use iroh::{
    EndpointAddr,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use tokio::{
    signal::unix::{SignalKind, signal},
    task::JoinHandle,
    time,
};

use crate::{
    agent::AgentEngines,
    brand,
    cli::{WORKER_GUIDE, WorkerArgs},
    config::Config,
    isolation::{
        IsolationStatus, TestbedIsolation, environment_identity, prepare,
        worker_protocol::{WORKER_ALPN, WorkerCall, WorkerReply, WorkerRequest},
    },
    logging,
    remote::{
        InstallationKey, LinkClient,
        protocol::{RemoteTicket, WorkerReport, builder, read_frame, write_frame},
    },
    version::VERSION,
};

use super::workers::HEARTBEAT_INTERVAL;

use layers::LinkLayers;

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
    let endpoint = builder()
        .secret_key(key.secret_key().clone())
        .alpns(vec![WORKER_ALPN.to_vec()])
        .bind()
        .await
        .map_err(io::Error::other)?;
    let client = LinkClient::from_endpoint(endpoint.clone());
    let master: RemoteTicket = args.master.parse().map_err(io::Error::other)?;
    let peer: EndpointAddr = master.into();
    // A host is spawned for one task and its whole host is the sandbox, so it
    // needs no nested microVM: the host is the boundary. The host engine is the
    // closest thing we have — a chroot over the machine, not a microVM.
    //
    // It is given the link too, because the software a task's sandbox expects is the
    // one thing worth fetching rather than building: an installs layer another of this
    // master's hosts built is restored here, and this host leaves its own for the next.
    let engine = TestbedIsolation::new(&base).with_host_layers(Arc::new(LinkLayers::new(
        client.clone(),
        peer.clone(),
        args.token.clone(),
        environment_identity().unwrap_or_else(|| layers::UNDECLARED.to_owned()),
    )));
    // Preparing is quick for a host engine, but it can still fail (no unprivileged
    // user namespaces, say). A host that cannot make sandboxes must not register and
    // take work it will fail, so it prepares before it dials the master.
    {
        let mut status = prepare(engine.clone());
        loop {
            match &*status.borrow_and_update() {
                IsolationStatus::Ready => {
                    tracing::info!("sandbox runtime ready");
                    break;
                }
                IsolationStatus::Failed(error) => {
                    return Err(io::Error::other(format!(
                        "the sandbox runtime could not be prepared: {error}"
                    ))
                    .into());
                }
                _ => {}
            }
            status.changed().await.map_err(io::Error::other)?;
        }
    }

    let service = WorkerService {
        engine,
        config: base.clone(),
        agents: AgentEngines::new(),
        token: args.token.clone(),
        client: client.clone(),
        master: peer.clone(),
        channels: Arc::new(Mutex::new(HashMap::new())),
        #[cfg(target_os = "linux")]
        caches: Arc::new(Mutex::new(HashMap::new())),
    };
    let worker_addr = endpoint.addr();
    let _router = Router::builder(endpoint)
        .accept(WORKER_ALPN, service)
        .spawn();

    let capacity = local_capacity();
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;

    'serving: loop {
        loop {
            match client
                .register_worker(
                    peer.clone(),
                    worker_addr.clone(),
                    &args.token,
                    &args.name,
                    capacity,
                )
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

#[derive(Clone)]
struct WorkerService {
    engine: TestbedIsolation,
    config: Config,
    // The same provider definitions the master builds recipes from, so a profile
    // id is enough to rebuild a recipe here.
    agents: AgentEngines,
    token: String,
    client: LinkClient,
    master: EndpointAddr,
    // The loopback listeners carrying each sandbox's agent channel to the master.
    channels: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    // The FUSE mount of each sandbox's project cache, kept alive until the sandbox
    // is gone and unmounted when it drops.
    #[cfg(target_os = "linux")]
    caches: Arc<Mutex<HashMap<String, BackgroundSession>>>,
}

impl fmt::Debug for WorkerService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkerService")
    }
}

impl ProtocolHandler for WorkerService {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        // The master keeps one connection to this host and sends its calls on it, so a
        // host answers each stream that arrives until the connection goes.
        loop {
            let Ok((send, recv)) = connection.accept_bi().await else {
                return Ok(());
            };
            let service = self.clone();
            tokio::spawn(async move {
                if let Err(error) = service.answer_call(send, recv).await {
                    tracing::warn!(%error, "a worker call went wrong");
                }
            });
        }
    }
}

// A call that went wrong in a way worth saying and then carrying on with.
type WorkerCallError = dyn Error + Send + Sync;

impl WorkerService {
    async fn answer_call(
        &self,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> Result<(), Box<WorkerCallError>> {
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
            call @ WorkerCall::Provision { .. } => {
                self.provision(send, recv, call).await;
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
            WorkerCall::Snapshot { id, checkout } => {
                self.send_snapshot(send, recv, id, checkout).await;
                Ok(())
            }
            WorkerCall::Restore { id, checkout } => {
                self.receive_snapshot(send, recv, id, checkout).await;
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

mod files;
mod layers;
mod service;

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
