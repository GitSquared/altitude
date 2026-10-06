use std::{error::Error, io};

use tokio::signal::unix::{SignalKind, signal};

use crate::{
    agent::AgentEngines,
    config::Config,
    db::Database,
    isolation::MicrosandboxIsolation,
    tasks::{TaskEventLoop, TaskService, TaskStore},
};

mod agent;
mod capacity;
mod config;
mod db;
mod development;
mod isolation;
mod logging;
mod net;
mod tasks;
mod transfer;
mod tui;
mod vcs;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
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
        let tasks = TaskService::new(
            TaskStore::new(&database),
            isolation,
            config.task_snapshots_dir(),
            AgentEngines::new(),
            identity,
        );
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
        let (event_loop, task_execution_updates) = TaskEventLoop::new(tasks.clone());
        // Recovered tasks are waiting, not running: they are admitted against what
        // the node can carry, like everything else.
        event_loop.admit();

        tui::run(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            event_loop,
            tasks,
            tui::App::new(task_rows, config.clone(), settings),
            task_execution_updates,
        )
        .await
    }
    .await;
    ratatui::restore();
    result?;
    Ok(())
}
