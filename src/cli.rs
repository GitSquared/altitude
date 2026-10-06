//! How the program is invoked.
//!
//! Altitude is one binary in two roles, and the role is not a preference: a
//! client wants a terminal and a person at it, a node wants neither. So the role
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
    /// Run as a node: serve work to clients, with no terminal.
    ///
    /// Reads its own settings file, keeps its own identity, and logs to stdout
    /// so a service manager can collect it.
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address and port to listen on, as `address:port`. Overrides the settings
    /// file. The port is UDP, because the link speaks QUIC.
    #[arg(long, value_name = "ADDR:PORT")]
    pub listen: Option<String>,

    /// Settings file to read instead of the default.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Data directory to use instead of the default, where the node keeps its
    /// key and its record of clients.
    #[arg(long, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,
}
