//! The single integration test binary of this crate.
//!
//! Every former `tests/<name>.rs` file is a module here instead of a test
//! binary of its own, and the helpers the tests share live in `common` once.
//! One binary per crate means the crate and its test support are compiled and
//! linked once per test run, which is what keeps the test build and CI short.
//!
//! The `insta` snapshots of those modules live in `tests/integration/snapshots`
//! and are named after the module path this binary gives them.

mod bookmarks;
mod browse;
mod catalog;
mod common;
mod cover_art;
mod download;
mod endpoints;
mod envelope;
mod genres;
mod now_playing;
mod play_queue;
mod playlists;
mod report_playback;
mod request;
mod scan;
mod scrobble;
mod search;
mod star;
mod stream;
mod stream_pending;
mod transcode;
mod users;
