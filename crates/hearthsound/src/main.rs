//! The Hearthsound server binary.
//!
//! The binary is a thin wrapper around the server's command line. At this stage
//! it only answers `--version`; subcommands arrive with the features they start.
#![forbid(unsafe_code)]

use clap::Parser;

/// The Hearthsound command line.
#[derive(Parser)]
#[command(
    name = "hearthsound",
    version,
    about = "Self-hosted music server",
    arg_required_else_help = true
)]
struct Cli {}

fn main() {
    Cli::parse();
}
