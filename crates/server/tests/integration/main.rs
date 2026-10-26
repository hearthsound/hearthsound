//! The single integration test binary of this crate.
//!
//! Every former `tests/<name>.rs` file is a module here instead of a test
//! binary of its own, and the helpers the tests share live in `common` once.
//! One binary per crate means the crate and its test support are compiled and
//! linked once per test run, which is what keeps the test build and CI short.

mod client_checklist;
mod common;
mod cors;
mod discovery_sync;
mod events;
mod health;
mod me_providers;
mod multi_user;
mod native;
mod on_demand;
mod playlists;
mod plugin_admin;
mod prefetch;
mod queue;
mod reload;
mod save_folder;
mod scrobbles;
mod setup;
mod storage;
mod stream_errors;
mod stream_pending;
mod vendor;
mod watch;
mod web;
mod web_session;
