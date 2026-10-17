//! The single integration test binary of this crate.
//!
//! The former `tests/<name>.rs` files are modules here instead of test binaries
//! of their own. One binary per crate means the crate is compiled and linked
//! once per test run, which is what keeps the test build and CI short.

mod acquisition_provider;
mod admin;
mod artwork;
mod artwork_provider;
mod catalog_provider;
mod deadline;
mod http_catalog;
mod install;
mod metadata_provider;
mod plugin_log;
mod provisioning;
