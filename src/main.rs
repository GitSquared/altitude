use std::{error::Error, io, sync::Arc, time::Duration};

use tokio::{
    signal::unix::{SignalKind, signal},
    time::{MissedTickBehavior, interval},
};

use clap::Parser;

use crate::{
    agent::AgentEngines,
    cli::{Cli, Command},
    config::Config,
    db::Database,
    isolation::MicrosandboxIsolation,
    remote::{InstallationKey, LinkClient, RemoteKeys},
    task::{TaskEventLoop, TaskService, TaskStore},
};

mod agent;
mod cli;
mod config;
mod db;
mod isolation;
mod logging;
mod remote;
mod task;
mod tracker;
mod tui;
mod vcs;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    match Cli::parse().command {
        Some(Command::Serve(args)) => remote::serve::run(args).await,
        None => client().await,
    }
}

/// The interactive client: a terminal, and a person at it.
async fn client() -> Result<(), Box<dyn Error>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let config = Config::load()?;
    let settings = config.load_or_create_settings();
    let _log_guard = logging::init(&config)?;
    tracing::info!("starting altitude");
    let mut terminal = ratatui::init();

    let result = async {
        let Some(database) = tui::boot_database(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            Database::open(&config),
        )
        .await?
        else {
            return Ok(());
        };
        // This installation's identity, named once and recorded on every task it
        // delegates.
        let identity = config.identity()?;
        let isolation = MicrosandboxIsolation::new(&config);
        let isolation_status = isolation::prepare(isolation.clone());
        let Some(_isolation_status) = tui::boot_isolation(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            isolation_status,
        )
        .await?
        else {
            return Ok(());
        };
        let tracker =
            match tracker::Linear::from_directory(&config.data_dir().join("trackers/linear")) {
                Ok(connection) => Some(Arc::new(connection) as Arc<dyn tracker::TrackerProvider>),
                Err(error) => {
                    tracing::warn!(%error, "Linear setup needs attention");
                    None
                }
            };
        let tasks = TaskService::new(
            TaskStore::new(&database),
            isolation,
            config.task_snapshots_dir(),
            AgentEngines::new(),
            identity,
            config.agent_channel_dir(),
        )
        .with_tracker(tracker);
        let Some(recovered) = tui::boot_task_resume(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            tasks.recover_all(),
        )
        .await?
        else {
            return Ok(());
        };
        for result in recovered {
            if let Err(error) = result {
                tracing::error!(%error, "could not recover a task");
            }
        }
        let task_rows = tasks.list().await.map_err(io::Error::other)?;
        // What each remote address last answered with. Read by the settings
        // panel, and written by the dial once there is one.
        let remote_keys = RemoteKeys::new(database.pool());
        let (event_loop, task_execution_updates) = TaskEventLoop::new(tasks.clone());
        // Recovered tasks are waiting, not running: they are admitted against what
        // the host can carry, like everything else.
        event_loop.admit();

        let link = LinkClient::bind(
            &InstallationKey::load_or_create(config.data_dir().join("identity.key"))
                .map_err(io::Error::other)?,
        )
        .await
        .map_err(io::Error::other)?;
        let syncing = tasks.clone();
        let tracker_sync = tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Err(error) = syncing.sync_tracker().await {
                    tracing::warn!(%error, "tracker updates remain queued");
                }
            }
        });
        let result = tui::run(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            event_loop,
            tasks,
            tui::App::new(
                task_rows,
                config.clone(),
                settings,
                remote_keys,
                link.clone(),
            ),
            task_execution_updates,
        )
        .await;
        tracker_sync.abort();
        link.shutdown().await;
        result
    }
    .await;
    ratatui::restore();
    result?;
    Ok(())
}
