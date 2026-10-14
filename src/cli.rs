//! How the program is invoked.
//!
//! Altitude is one binary in two roles, and the role is not a preference: a
//! client wants a terminal and a person at it, a server wants neither. So the role
//! is chosen on the command line rather than inferred from a config file that
//! one role cannot sensibly read anyway.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// A personal, headless intent control plane for delegated coding work.
#[derive(Debug, Parser)]
#[command(name = "altitude", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run as a server: serve work to clients, with no terminal.
    ///
    /// Reads its own settings file, keeps its own identity, and logs to stdout
    /// so a service manager can collect it.
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Install the shared Linear app, then exit. Reads the Linear settings in serve.toml.
    #[arg(long)]
    pub connect_linear: bool,

    /// Address and port to listen on, as `address:port`. Overrides the settings
    /// file. The port is UDP, because the link speaks QUIC.
    #[arg(long, value_name = "ADDR:PORT")]
    pub listen: Option<String>,

    /// Settings file to read instead of the default.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Data directory to use instead of the default, where the serve role keeps its
    /// key and its record of clients.
    #[arg(long, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,
}
