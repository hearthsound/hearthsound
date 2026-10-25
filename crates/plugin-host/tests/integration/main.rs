//! The single integration test binary of this crate.
//!
//! The former `tests/<name>.rs` files are modules here instead of test binaries
//! of their own. One binary per crate means the crate is compiled and linked
//! once per test run, which is what keeps the test build and CI short.

mod account_link_provider;
mod acquisition_provider;
mod admin;
mod artwork;
mod artwork_provider;
mod catalog_provider;
mod deadline;
mod discovery_provider;
mod distribution;
mod http_catalog;
mod install;
mod kv;
mod legacy;
mod metadata_provider;
mod plugin_log;
mod provisioning;
mod repositories;
mod scrobble_provider;
mod user_configs;
mod verification_provider;
