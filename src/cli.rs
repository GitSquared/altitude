//! How the program is invoked.
//!
//! Altitude is one binary in two roles, and the role is not a preference: a
//! client wants a terminal and a person at it, a server wants neither. So the role
//! is chosen on the command line rather than inferred from a config file that
//! one role cannot sensibly read anyway.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::{tracker::TrackerKind, version::VERSION};

const SERVE_MODE: &str = "Run altitude serve on a headless server to let your team run agents beyond their own machines, and pilot them directly from your task tracker.";

const ROOT_HELP_TEMPLATE: &str = "\
{name} {version}
{about}

{usage-heading} {usage}

{all-args}{after-help}";

#[derive(Debug, Parser)]
#[command(
    name = "altitude",
    display_name = "⛰  altitude",
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
}

/// What a tracker-integrated remote needs, which is the same shape for any
/// provider: an app its owner administers, an address that reaches this machine,
/// and one command that installs it here.
const SERVE_AFTER_HELP: &str = "\
A remote can run work delegated from a task tracker. That needs, on this machine:
\x20 1. an OAuth application in the tracker, owned by whoever administers it here
\x20 2. an HTTPS address that reaches this machine, with a reverse proxy to the
\x20    tracker's webhook port (127.0.0.1:25849 by default)
\x20 3. the application installed once, by `altitude serve connect linear`

Without one, this remote still runs work sent from Altitude, and reports through
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
Altitude shows anywhere, but the addresses, ports and logins are this host's own.";

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Settings file to read instead of the default `~/.config/altitude/serve.toml`.
    ///
    /// `$XDG_CONFIG_HOME/altitude/serve.toml` and `$ALTITUDE_CONFIG_DIR/serve.toml`
    /// are where that default comes from, so setting either moves it.
    #[arg(long, value_name = "PATH", display_order = 1)]
    pub config: Option<PathBuf>,

    /// Address and port to listen on, as `address:port`. Defaults to
    /// `0.0.0.0:25848`, with discovery over TCP and the task connection over UDP.
    /// Overrides the settings file.
    #[arg(long, value_name = "ADDR:PORT", display_order = 2)]
    pub listen: Option<String>,

    /// Data directory to use instead of the default, where the serve role keeps its
    /// key and its record of clients.
    #[arg(long, value_name = "PATH", display_order = 3)]
    pub data_dir: Option<PathBuf>,

    /// Something to do instead of serving.
    ///
    /// Housekeeping and installing belong to this role rather than to a run of it,
    /// so they are subcommands: a run has options, and these do not serve at all.
    #[command(subcommand)]
    pub command: Option<ServeCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ServeCommand {
    Clients,
    Evict {
        key: String,
    },
    /// Install Altitude as a service on this machine, then exit
    ///
    /// Asks whether it should run for this user or for the whole machine, then
    /// starts it. Needs a terminal, and writes nothing before you agree.
    Install,
    /// Remove the service this machine runs, keeping everything it holds
    ///
    /// Finds what is installed here, stops it and removes it, and leaves the
    /// tasks, logins and settings alone. Needs a terminal.
    Uninstall,
    /// Install a task tracker's shared app on this machine, then exit
    ///
    /// Prints what this machine holds, walks through the application's setup steps,
    /// and saves the login here.
    #[command(after_help = CONNECT_AFTER_HELP)]
    Connect(ConnectArgs),
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
