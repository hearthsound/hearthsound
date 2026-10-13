//! Helpers for integration tests in this workspace.
//!
//! The crate locates the workspace root and the fixture plugins built by
//! `cargo xtask fixtures`, so integration tests do not have to repeat that path
//! arithmetic, and it synthesizes audio files with tags and cover art at test
//! time through the [`audio`] module, so the repository never contains
//! copyrighted audio.
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub mod audio;

use std::path::PathBuf;

/// Returns the root of the workspace.
///
/// # Returns
///
/// The directory holding the workspace `Cargo.toml`.
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Returns the path of a fixture plugin archive.
///
/// # Parameters
///
/// - `file_name`: the archive file name, for example [`HELLO_CATALOG`].
///
/// # Returns
///
/// The absolute path of the archive under `target/fixtures/plugins/`.
///
/// # Panics
///
/// Panics when the archive does not exist, because that means the fixtures were
/// never built.
pub fn fixture_plugin(file_name: &str) -> PathBuf {
    let path = workspace_root()
        .join("target/fixtures/plugins")
        .join(file_name);
    assert!(
        path.is_file(),
        "fixture plugin {} is missing; run cargo xtask fixtures first",
        path.display()
    );
    path
}

/// The archive file name of the hello catalog fixture.
pub const HELLO_CATALOG: &str = "com.example.hello-catalog-0.1.0.hsplugin";

/// The archive file name of the spin catalog fixture.
pub const SPIN_CATALOG: &str = "com.example.spin-catalog-0.1.0.hsplugin";

/// The archive file name of the HTTP catalog fixture.
pub const HTTP_CATALOG: &str = "com.example.http-catalog-0.1.0.hsplugin";

/// The archive file name of the metadata fixture.
pub const METADATA_FIXTURE: &str = "com.example.metadata-fixture-0.1.0.hsplugin";

/// The archive file name of the acquisition fixture.
pub const ACQUISITION_FIXTURE: &str = "com.example.acquisition-fixture-0.1.0.hsplugin";
