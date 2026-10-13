//! The command line tree.
//!
//! Every command is a clap type in this module and an implementation in
//! [`crate::commands`]. The tree is the documented surface of the product: a
//! command that appears in `--help` does what its help says, and a command that
//! cannot do what it says does not ship.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The Hearthsound command line.
#[derive(Debug, Parser)]
#[command(
    name = "hearthsound",
    version,
    about = "Self-hosted music server",
    arg_required_else_help = true
)]
pub struct Cli {
    /// The TOML configuration file to read.
    ///
    /// A missing file is not an error: every bootstrap key can also come from a
    /// `HEARTHSOUND__SECTION__KEY` environment variable, which wins over the
    /// file.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Every command the binary understands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the server.
    Serve,
    /// Apply pending database migrations.
    ///
    /// A database that already holds data is copied into `<data>/backups` with
    /// `VACUUM INTO` before the migrations run, and the command reports the
    /// version it started at and the version it ended at.
    Migrate,
    /// Index the tags of the files in the library folders.
    Scan(ScanArgs),
    // The acquisition commands (`acquisitions list` and `acquisitions cancel`)
    // are registered here once the acquisition orchestrator exists.
}

/// The arguments of `scan`.
#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Scan one folder instead of every folder.
    ///
    /// The value is either a library folder id or the path of a folder that was
    /// added with `admin add-folder`.
    #[arg(long, value_name = "PATH-OR-ID")]
    pub folder: Option<String>,
}
