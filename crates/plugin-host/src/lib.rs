//! The WebAssembly plugin host.
//!
//! A Hearthsound plugin is a WebAssembly component that speaks the
//! `hearthsound:plugin@0.1.0` package. This crate reads plugin archives,
//! validates that a component only imports what the host offers and only
//! exports the interfaces its manifest declares, and installs it under the data
//! directory, keeping a precompiled artifact so loading does not recompile.
//!
//! The crate contains one `unsafe` call: deserializing a precompiled component
//! from the host's own data directory. Every other operation is safe Rust.
#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod archive;
mod engine;
mod error;
mod host;
pub mod manifest;
mod validate;

pub use archive::PluginArchive;
pub use engine::PluginEngine;
pub use error::{HostError, InstallError};
pub use host::{InstalledPlugin, PluginHost};
pub use manifest::{Manifest, ManifestError};
