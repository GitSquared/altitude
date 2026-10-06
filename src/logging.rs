use std::{fs, io};

use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::Config;

/// Logging for the serve role: to stdout, because a service manager collects it
/// and nobody is watching a terminal.
///
/// A different shape from [`init`] on purpose. There is no file to roll and no
/// guard to hold, and the serve role's output belongs wherever the operator's service
/// manager put it.
pub fn init_service() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("altitude=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_ansi(false))
        .init();
}

pub fn init(config: &Config) -> io::Result<WorkerGuard> {
    fs::create_dir_all(config.logs_dir())?;
    let appender = rolling::never(config.logs_dir(), "altitude.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("altitude=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_ansi(false).with_writer(writer))
        .init();
    Ok(guard)
}
