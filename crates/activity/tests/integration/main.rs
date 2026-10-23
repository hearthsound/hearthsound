//! The single integration test binary of this crate.
//!
//! The former `tests/plays.rs` file is a module here instead of a test binary
//! of its own. One binary per crate means the crate is compiled and linked once
//! per test run, which is what keeps the test build and CI short.

mod annotations;
mod bookmarks;
mod common;
mod discovery_sync;
mod dislikes;
mod feedback;
mod library;
mod play_queues;
mod playlists;
mod plays;
mod ratings;
mod reports;
mod scrobbles;
