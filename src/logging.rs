use std::{
    env,
    fmt::{Debug, Result as FormatResult},
    fs, io,
    io::IsTerminal,
};

use tracing::field::Field;
use tracing_appender::{non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::Config;

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
                .fmt_fields(fmt::format::debug_fn(serve_field))
                .with_writer(io::stdout),
        )
        .init();
}

fn serve_field(
    writer: &mut fmt::format::Writer<'_>,
    field: &Field,
    value: &dyn Debug,
) -> FormatResult {
    let text = format!("{value:?}").replace('\u{1b}', "\\x1b");
    let color = writer.has_ansi_escapes();
    if field.name() == "message" {
        if color {
            write!(writer, "\x1b[1m{text}\x1b[0m")
        } else {
            write!(writer, "{text}")
        }
    } else if color {
        write!(writer, " \x1b[2m{}=\x1b[0;36m{text}\x1b[0m", field.name())
    } else {
        write!(writer, " {}={text}", field.name())
    }
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
