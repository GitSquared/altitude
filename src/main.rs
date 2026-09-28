use std::{error::Error, io};

use tokio::signal::unix::{SignalKind, signal};

use crate::{
    agent::ClaudeCode,
    config::Config,
    db::Database,
    isolation::MicrosandboxIsolation,
    tasks::{TaskEventLoop, TaskService, TaskStore},
};

mod agent;
mod config;
mod db;
mod development;
mod isolation;
mod logging;
mod tasks;
mod tui;
mod vcs;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let config = Config::load()?;
    let settings = config.load_or_create_settings()?;
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
        let claude = ClaudeCode;

        let tasks = TaskService::new(
            TaskStore::new(&database),
            isolation,
            config.task_snapshots_dir(),
            claude,
        );
        let Some(resumed) = tui::boot_task_resume(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            tasks.resume_all(),
        )
        .await?
        else {
            return Ok(());
        };
        let task_rows = tasks.list().await.map_err(io::Error::other)?;
        let (event_loop, task_execution_updates) = TaskEventLoop::new(tasks.clone());
        event_loop.watch_resumed_runs(resumed);

        tui::run(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            event_loop,
            tasks,
            config.clone(),
            settings.environments.clone(),
            settings.default_environment.clone(),
            task_rows,
            task_execution_updates,
        )
        .await
    }
    .await;
    ratatui::restore();
    result?;
    Ok(())
}
