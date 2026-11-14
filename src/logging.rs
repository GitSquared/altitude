use std::{
    env,
    fmt::{Debug, Result as FormatResult},
    fs, io,
    io::IsTerminal,
    sync::{Mutex, OnceLock},
};

use tokio::sync::mpsc;

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
        .with(fmt::layer().with_ansi(false).with_writer(Relay))
        .init();
}

// While a host is answering a call, its log also goes to the master: the machine that
// asked is the only place a failure on a machine it cannot see can be read. Outside a
// call this is a sink that takes the line and drops it.
static RELAY: OnceLock<Mutex<Option<mpsc::UnboundedSender<String>>>> = OnceLock::new();

pub fn relay(lines: Option<mpsc::UnboundedSender<String>>) {
    let relay = RELAY.get_or_init(|| Mutex::new(None));
    if let Ok(mut current) = relay.lock() {
        *current = lines;
    }
}

struct Relay;

impl io::Write for Relay {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(relay) = RELAY.get()
            && let Ok(current) = relay.lock()
            && let Some(lines) = current.as_ref()
        {
            let _ = lines.send(String::from_utf8_lossy(bytes).into_owned());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> fmt::MakeWriter<'a> for Relay {
    type Writer = Relay;

    fn make_writer(&'a self) -> Self::Writer {
        Relay
    }
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
