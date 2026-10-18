//! The integration tests of the native API crate.
//!
//! The tests here do not start a server: they exercise the document the crate
//! publishes, which is the contract `crates/server` serves and the web client
//! generates its types from.

mod openapi;
