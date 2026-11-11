use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::{tracker::TrackerKind, version::VERSION};

const SERVE_MODE: &str = "Run altitude serve on a headless server to let your team run agents beyond their own machines, and pilot them directly from your task tracker.";

const POSTER_MODE: &str =
    "Render the terrain altitude opens with, fullscreen and moving, until q or Esc quits.";

const ROOT_HELP_TEMPLATE: &str = "\
{name} {version}
{about}

{usage-heading} {usage}

{all-args}{after-help}";

#[derive(Debug, Parser)]
#[command(
    name = "altitude",
    display_name = crate::brand::TITLE,
    version = VERSION,
    about = env!("CARGO_PKG_DESCRIPTION"),
    long_about = format!(
        "{}\n\nStart altitude without any argument for the interactive TUI application to manage tasks and agents.\n\n{SERVE_MODE}", env!("CARGO_PKG_DESCRIPTION")
    ),
    help_template = ROOT_HELP_TEMPLATE,
    max_term_width = 90
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(long_about = SERVE_MODE, after_help = SERVE_AFTER_HELP)]
    Serve(ServeArgs),
    #[command(long_about = POSTER_MODE)]
    Poster,
}

const SERVE_AFTER_HELP: &str = "\
A remote can run work delegated from a task tracker. That needs, on this machine:
\x20 1. an OAuth application in the tracker, owned by whoever administers it here
\x20 2. an HTTPS address that reaches this machine, with a reverse proxy to the
\x20    tracker's webhook port (127.0.0.1:25849 by default)
\x20 3. the application installed once, by `altitude serve connect linear`

Without one, this remote still runs work sent from altitude, and reports through
each operator's own login.

Set LOG_LEVEL=debug for detailed logs. Terminal colors are automatic; set NO_COLOR to disable them.";

const CONNECT_AFTER_HELP: &str = "\
The steps:
\x20 1. Choose the HTTPS address that reaches the remote.
\x20 2. Create the tracker's OAuth application with that address's redirect
\x20    and webhook URLs.
\x20 3. Put its client ID, secrets, and that address in serve.toml.
\x20 4. Route the two URLs from that address to the remote.
\x20 5. Run this command on the remote.

Every fact it prints belongs to this machine: the steps are the same ones
altitude shows anywhere, but the addresses, ports and logins are this host's own.";

#[derive(Debug, Args)]
pub struct ServeArgs {
    #[arg(long, value_name = "PATH", display_order = 1)]
    pub config: Option<PathBuf>,

    #[arg(long, value_name = "ADDR:PORT", display_order = 2)]
    pub listen: Option<String>,

    #[arg(long, value_name = "PATH", display_order = 3)]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<ServeCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ServeCommand {
    Clients,
    Evict {
        key: String,
    },
    Install,
    Uninstall,
    #[command(after_help = CONNECT_AFTER_HELP)]
    Connect(ConnectArgs),
    #[command(long_about = WORKER_MODE, after_help = WORKER_AFTER_HELP)]
    Worker(WorkerArgs),
}

const WORKER_MODE: &str =
    "Run altitude serve as an empty box that supplies compute to one `altitude serve` master.";

const WORKER_AFTER_HELP: &str = "\
The master provisions this process with a provision script. Run by hand only to \
debug that script: the master generates the token, and the ticket and name it \
passes are the ones to use here.";

#[derive(Debug, Args)]
pub struct WorkerArgs {
    #[arg(
        long,
        value_name = "TICKET",
        display_order = 1,
        help = "The master's address ticket, from its worker settings or debug log"
    )]
    pub master: String,

    #[arg(
        long,
        value_name = "TOKEN",
        display_order = 2,
        help = "The master's worker token, from its data directory if not given in the script"
    )]
    pub token: String,

    #[arg(
        long,
        value_name = "NAME",
        display_order = 3,
        help = "The name this box was provisioned under"
    )]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ConnectArgs {
    #[arg(value_enum)]
    pub tracker: Tracker,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Tracker {
    Linear,
}

impl From<Tracker> for TrackerKind {
    fn from(tracker: Tracker) -> Self {
        match tracker {
            Tracker::Linear => Self::Linear,
        }
    }
}
