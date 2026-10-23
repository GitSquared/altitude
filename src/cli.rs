//! How the program is invoked.
//!
//! Altitude is one binary in two roles, and the role is not a preference: a
//! client wants a terminal and a person at it, a server wants neither. So the role
//! is chosen on the command line rather than inferred from a config file that
//! one role cannot sensibly read anyway.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The serve mode, written once so the root's long help and the subcommand agree.
const SERVE_MODE: &str = "Run altitude serve on a headless server to let your team run agents beyond their own machines, and pilot them directly from your task tracker.";

/// The root help opens with the same icon, name, and version as `--version`.
const ROOT_HELP_TEMPLATE: &str = "\
{name} {version}
{about}

{usage-heading} {usage}

{all-args}{after-help}";

#[derive(Debug, Parser)]
#[command(
    name = "altitude",
    display_name = "⛰  altitude",
    version = concat!("v", env!("CARGO_PKG_VERSION")),
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
    /// Run as a headless remote hosting agents for clients
    #[command(
        long_about = SERVE_MODE,
        after_help = "Set LOG_LEVEL=debug for detailed logs. Terminal colors are automatic; set NO_COLOR to disable them."
    )]
    Serve(ServeArgs),
}

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

    /// Install the shared Linear app, then exit. Reads the Linear settings in serve.toml.
    #[arg(long, help_heading = "Task tracker integration")]
    pub connect_linear: bool,

    /// What to do instead of running the remote: look after the clients it has seen.
    #[command(subcommand)]
    pub command: Option<ServeCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ServeCommand {
    /// List the clients that have used this remote, and what each one pushed
    Clients,
    /// Forget a client: its pushed configuration, its logins, and its tasks
    Evict {
        /// The client's public key, as `clients` prints it
        key: String,
    },
}
