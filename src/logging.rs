use std::io;

use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::Config;

pub fn init(config: &Config) -> io::Result<WorkerGuard> {
    std::fs::create_dir_all(config.logs_dir())?;
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
