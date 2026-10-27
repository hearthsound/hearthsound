//! The single integration test binary of this crate.
//!
//! Every former `tests/<name>.rs` file is a module here instead of a test
//! binary of its own, and the helpers the tests share live in `common` once.
//! One binary per crate means the crate and its test support are compiled and
//! linked once per test run, which is what keeps the test build and CI short.

mod common;
mod deadline;
mod fields;
mod identity;
mod index;
mod merge;
mod search;
