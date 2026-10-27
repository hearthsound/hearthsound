//! The version-specific host layer.
//!
//! Every generated type belongs to one plugin API version, so the adapters, the
//! host imports and their bindings live in one tree per version:
//!
//! - [`v5`] drives `hearthsound:plugin@0.5.0`, the current package in `wit/`;
//! - [`v4`] drives the frozen `hearthsound:plugin@0.4.0` package in
//!   `wit/since_v0.4/`.
//!
//! Everything that enforces policy stays in the shared modules: the store pool,
//! the permission checks inside the imports, the process table, the file grants
//! and the manifest rules. A fix there therefore applies to every hosted
//! version, and only a change to the interfaces themselves reaches this
//! directory.

pub(crate) mod v4;
pub(crate) mod v5;
