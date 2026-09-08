use std::error::Error;

use tokio::signal::unix::{SignalKind, signal};

use crate::{config::Config, db::Database, isolation::MicrosandboxIsolation};

mod config;
mod db;
mod isolation;
mod tui;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let config = Config::load()?;
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
        let _pool = database.pool();

        let isolation = MicrosandboxIsolation::new(&config);
        let isolation_status = isolation::prepare(isolation.clone());
        let Some(isolation_status) = tui::boot_isolation(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            isolation_status,
        )
        .await?
        else {
            return Ok(());
        };

        tui::run(
            &mut terminal,
            &mut terminate,
            &mut interrupt,
            isolation_status,
            isolation,
        )
        .await
    }
    .await;
    ratatui::restore();
    result?;
    Ok(())
}
