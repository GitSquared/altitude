use std::{env, fs, io, io::IsTerminal};

use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::Config;

/// Serve logs stay on stdout so terminals and service managers share one stream.
pub fn init_service() {
    let terminal = io::stdout().is_terminal();
    let color = terminal
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").is_ok_and(|term| term != "dumb");
    let filter =
        EnvFilter::try_from_env("LOG_LEVEL").unwrap_or_else(|_| EnvFilter::new("altitude=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(!terminal)
                .with_ansi(color)
                .with_writer(io::stdout),
        )
        .init();
}

pub fn init(config: &Config) -> io::Result<WorkerGuard> {
    fs::create_dir_all(config.logs_dir())?;
    let appender = rolling::never(config.logs_dir(), "altitude.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter =
        EnvFilter::try_from_env("LOG_LEVEL").unwrap_or_else(|_| EnvFilter::new("altitude=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_ansi(false).with_writer(writer))
        .init();
    Ok(guard)
}
