//! The command implementations.
//!
//! Each module is one command or one command group. A command resolves what it
//! needs from the [`Context`](crate::context::Context) or from
//! `hearthsound-server`, prints what it did, and returns an `anyhow::Result`
//! that `main` renders as a message and an exit code.

pub mod migrate;
pub mod scan;
pub mod serve;
