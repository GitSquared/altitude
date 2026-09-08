use std::error::Error;

use tokio::signal::unix::{SignalKind, signal};

use crate::isolation::MicrosandboxIsolation;

mod isolation;
mod tui;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let isolation = MicrosandboxIsolation::new()?;
    let isolation_status = isolation::prepare(isolation);
    let mut terminal = ratatui::init();

    let result = tui::run(
        &mut terminal,
        &mut terminate,
        &mut interrupt,
        isolation_status,
    )
    .await;
    ratatui::restore();
    result?;
    Ok(())
}
