//! The single integration test binary of this crate.
//!
//! The former `tests/<name>.rs` files are modules here instead of test binaries
//! of their own. One binary per crate means the crate is compiled and linked
//! once per test run, which is what keeps the test build and CI short.

mod large_library;
mod maintenance;
mod naming;
mod query;
mod repository;
mod retention;
mod scan;
mod watch;
