//! The WebAssembly plugin host.
//!
//! A Hearthsound plugin is a WebAssembly component that speaks a
//! `hearthsound:plugin` API package: `hearthsound:plugin@0.4.0`, the newest one
//! in `wit/`, and `hearthsound:plugin@0.3.0`, the previous minor the host still
//! loads from `wit/since_v0.3/`. This crate reads plugin archives,
//! validates that a component only imports what the host offers and only
//! exports the interfaces its manifest declares, installs it under the data
//! directory, and runs it inside a pool of wasmtime stores with deadlines and
//! host imports that deny everything the plugin was not granted.
//!
//! The rest of the server never sees WebAssembly: [`PluginHost::catalog_provider`]
//! hands out a core trait object whose calls cross the guest boundary and back.
//! [`ProviderRegistry`] is how the server finds those objects: it is built from
//! the plugin tables, ordered by the administrator's layer assignments, and
//! rebuilt when the tables change. [`InstanceService`] is the write side of the
//! same tables - creating an instance validates its configuration against the
//! plugin's schema, seals its secrets, and asks the plugin whether the
//! configuration works before anything is stored.
//!
//! Every host import enforces the instance's granted permissions itself.
//! [`http`] is the outbound HTTP import: it applies the instance's host
//! allowlist, address policy, timeout, redirect, body-cap and rate-limit rules
//! before any request leaves the process.
//!
//! The crate contains one `unsafe` call: deserializing a precompiled component
//! from the host's own data directory. Every other operation is safe Rust.
#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod admin;
mod api;
pub mod archive;
mod defaults;
mod engine;
mod error;
pub mod files;
mod host;
pub mod http;
pub mod index;
pub mod install;
mod installer;
mod instance;
pub mod instances;
pub mod kv;
pub mod log;
pub mod manifest;
mod pool;
mod process;
pub mod registry;
mod repo;
mod state;
mod updates;
mod user_configs;
mod validate;

pub use admin::{AdminError, PluginAdmin};
pub use archive::PluginArchive;
pub use defaults::default_plugin_ids;
pub use engine::PluginEngine;
pub use error::{HostError, InstallError};
pub use files::{FileAccess, FileStat, FilesError};
pub use host::{InstalledPlugin, PluginHost};
pub use index::{Index, IndexClient, IndexError, IndexPlugin, IndexVersion, Unavailable};
pub use install::{
    PluginRegistration, RequestedPermission, Risk, new_permissions, requested_permissions,
    requested_permissions_of,
};
pub use installer::{
    AvailableError, AvailablePlugin, AvailableReport, PluginInstaller, RepositoryRef,
};
pub use instance::{ImportRoot, InstanceConfig, InstanceHostConfig};
pub use instances::{
    InstanceError, InstanceService, InstanceSpec, InstanceView, SECRET_PLACEHOLDER,
};
pub use kv::KvStore;
pub use log::{GuestOutput, LogSink};
pub use manifest::{Manifest, ManifestError};
pub use registry::{
    FetchError, FetchedImage, HealthOutcome, ProviderRegistry, RegistryInstance, RegistryReport,
    SkippedInstance,
};
pub use repo::{
    Assignment, InstanceRow, PluginRepository, PluginRow, PluginSource, PluginState, RepoError,
    RepositoryRow,
};
pub use state::InstanceState;
pub use updates::{
    PluginUpdate, UPDATE_CHECK_JOB_KIND, UPDATE_CHECK_SCHEDULE, UpdateCheckJobHandler,
    UpdateCheckReport, UpdateChecker,
};
pub use user_configs::{
    LinkedUser, UserConfig, UserConfigError, UserConfigService, merge_user_config,
    redact_user_config, validate_user_config,
};
pub use validate::HOSTED_LAYERS;
