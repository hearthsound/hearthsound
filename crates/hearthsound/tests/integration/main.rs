//! The single integration test binary of this crate.
//!
//! The former `tests/cli.rs` file is a module here instead of a test binary of
//! its own. One binary per crate means the crate and the command line it builds
//! are compiled and linked once per test run, which is what keeps the test build
//! and CI short.

mod cli;
