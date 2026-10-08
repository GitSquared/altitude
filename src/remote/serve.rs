//! The serve role: work for clients, with no terminal.
//!
//! A different execution mode, not a different configuration of the same one. A
//! serve role has nobody at the keyboard, so it never takes the terminal, it logs to
//! stdout for whatever service manager started it, and it reads its own settings
//! file rather than the client's.

use std::{error::Error, io, net::SocketAddr};

use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

use crate::{
    agent::AgentEngines,
    cli::ServeArgs,
    config::{Config, ServeSettings},
    isolation::{
        self, EnvironmentProgress, EnvironmentProgressSink, IsolationStatus, MicrosandboxIsolation,
    },
    logging,
    remote::{ClientRecord, InstallationKey, LinkServer, announce::RemoteState},
    vcs::GitHub,
};

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
    let server = LinkServer::serve(
        identity,
        clients,
        listen,
        label,
        config.settings_path(),
        observed,
    )
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
