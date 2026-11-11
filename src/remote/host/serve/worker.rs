use std::{error::Error, io, io::IsTerminal, time::Duration};

use iroh::EndpointAddr;
use tokio::{
    signal::unix::{SignalKind, signal},
    time,
};

use crate::{
    brand,
    cli::{WORKER_GUIDE, WorkerArgs},
    config::Config,
    logging,
    remote::{
        InstallationKey, LinkClient,
        protocol::{RemoteTicket, WorkerReport},
    },
    version::VERSION,
};

use super::workers::HEARTBEAT_INTERVAL;

// The other end of the fleet contract: an empty box that knows nothing but who
// made it. It registers, says it is still there, and waits to be given sandboxes.
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
    let client = LinkClient::bind(&key).await.map_err(io::Error::other)?;
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
