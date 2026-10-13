//! The WebAssembly plugin host.
//!
//! A Hearthsound plugin is a WebAssembly component that speaks the
//! `hearthsound:plugin@0.1.0` package. This crate reads plugin archives,
//! validates that a component only imports what the host offers and only
//! exports the interfaces its manifest declares, installs it under the data
//! directory, and runs it inside a pool of wasmtime stores with deadlines and
//! host imports that deny everything the plugin was not granted.
//!
//! The rest of the server never sees WebAssembly: [`PluginHost::catalog_provider`]
//! hands out a core trait object whose calls cross the guest boundary and back.
//!
//! Every host import enforces the instance's approved permissions itself.
//! [`http`] is the outbound HTTP import: it applies the instance's host
//! allowlist, address policy, timeout, redirect, body-cap and rate-limit rules
//! before any request leaves the process.
//!
//! The crate contains one `unsafe` call: deserializing a precompiled component
//! from the host's own data directory. Every other operation is safe Rust.
#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod acquisition;
pub mod archive;
mod bindings;
mod catalog;
mod engine;
mod error;
pub mod files;
mod host;
pub mod http;
mod instance;
pub mod kv;
pub mod log;
pub mod manifest;
mod metadata;
mod pool;
mod state;
mod validate;

pub use archive::PluginArchive;
pub use engine::PluginEngine;
pub use error::{HostError, InstallError};
pub use files::{FileAccess, FileStat, FilesError};
pub use host::{InstalledPlugin, PluginHost};
pub use instance::{ImportRoot, InstanceConfig, InstanceHostConfig};
pub use kv::KvStore;
pub use log::{GuestOutput, LogSink};
pub use manifest::{Manifest, ManifestError};
pub use state::InstanceState;
