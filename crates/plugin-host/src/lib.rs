//! The WebAssembly plugin host.
//!
//! A Hearthsound plugin is a WebAssembly component that speaks the
//! `hearthsound:plugin@0.1.0` package. This crate reads plugin archives and
//! validates their manifests; the component validation, install, and runtime
//! land here as well.
#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod manifest;

pub use manifest::{Manifest, ManifestError};
