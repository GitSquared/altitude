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
    #[command(long_about = WORKER_MODE, after_help = WORKER_GUIDE)]
    Worker(WorkerArgs),
}

const WORKER_MODE: &str =
    "Run altitude serve as a disposable host that runs one task's sandbox for a remote.";

// Shown as the worker subcommand's help, and printed by the worker itself when it
// is launched from a terminal, so that a stray invocation reads this rather than
// turning the machine it was typed on into a host.
pub(crate) const WORKER_GUIDE: &str = "\
A worker is a host a remote spawns for one task: the whole host is the sandbox, so
nothing inside it needs a nested isolation. The remote does not provision it
itself; it runs the scripts in serve.toml, which are handed:

  ALTITUDE_MASTER_TICKET   the address to dial (this worker's --master)
  ALTITUDE_WORKER_TOKEN    the shared secret (this worker's --token)
  ALTITUDE_WORKER_NAME     the name it registers under (this worker's --name)

To set workers up on a remote:

 1. Write a provision script that brings a machine up and starts a worker on it:
      altitude serve worker --master \"$ALTITUDE_MASTER_TICKET\" \\
        --token \"$ALTITUDE_WORKER_TOKEN\" --name \"$ALTITUDE_WORKER_NAME\"
    It exits 0 once that host can reach the remote. A worker refuses to run in a
    terminal, so a provision script cannot leave a person looking at a prompt.
 2. Write a deprovision script that destroys the host named by
    ALTITUDE_WORKER_NAME and exits 0 once it is gone.
 3. Point the remote's serve.toml at both, with the most it may run at once:
      [workers]
      provision = \"/etc/altitude/provision-worker.sh\"
      deprovision = \"/etc/altitude/deprovision-worker.sh\"
      max = 8
 4. Stock the host image: the host is the sandbox, so it carries what a task needs
    (git, the agent harness) or has the network to install it, and it can reach the
    remote.
 5. Recommended: bake a warm copy of each project's environment and caches into the
    image, so a start does not rebuild them.

The remote spawns a host when a task needs one and destroys it when the task ends.
Run this by hand only to debug a provision script.";

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
        help = "The name this host was provisioned under"
    )]
    pub name: String,

    #[arg(
        long,
        display_order = 4,
        help = "Run even from a terminal, for debugging a provision script"
    )]
    pub yes: bool,
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
