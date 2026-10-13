//! Reading `.hsplugin` archives.
//!
//! An archive is a zip holding at least `manifest.json` and `plugin.wasm`, plus
//! optional documentation and icon files. Reading an archive computes its
//! SHA-256 and bounds every entry, so a hostile archive cannot make the host
//! allocate without limit.

use std::io::{Cursor, Read};
use std::path::Path;

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::error::InstallError;

/// The largest accepted `manifest.json`.
const MANIFEST_LIMIT: u64 = 1024 * 1024;

/// The largest accepted `plugin.wasm`.
const WASM_LIMIT: u64 = 128 * 1024 * 1024;

/// The largest accepted optional file.
const OPTIONAL_LIMIT: u64 = 1024 * 1024;

/// The contents of one plugin archive.
#[derive(Debug)]
pub struct PluginArchive {
    sha256: [u8; 32],
    manifest: Vec<u8>,
    wasm: Vec<u8>,
    readme: Option<Vec<u8>>,
    license: Option<Vec<u8>>,
    icon: Option<Vec<u8>>,
}

impl PluginArchive {
    /// Reads and verifies the structure of the archive at `path`.
    ///
    /// # Parameters
    ///
    /// - `path`: the `.hsplugin` file.
    ///
    /// # Returns
    ///
    /// The archive contents.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::Io`] when the file cannot be read,
    /// [`InstallError::Archive`] when it is not a zip,
    /// [`InstallError::MissingEntry`] when a required entry is absent, and
    /// [`InstallError::EntryTooLarge`] when an entry exceeds its limit.
    pub fn read(path: &Path) -> Result<PluginArchive, InstallError> {
        let bytes = std::fs::read(path).map_err(|source| InstallError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(Sha256::digest(&bytes).as_slice());

        let mut zip = ZipArchive::new(Cursor::new(bytes))?;
        let manifest = read_entry(&mut zip, "manifest.json", MANIFEST_LIMIT)?
            .ok_or(InstallError::MissingEntry("manifest.json"))?;
        let wasm = read_entry(&mut zip, "plugin.wasm", WASM_LIMIT)?
            .ok_or(InstallError::MissingEntry("plugin.wasm"))?;
        let readme = read_entry(&mut zip, "README.md", OPTIONAL_LIMIT)?;
        let license = read_entry(&mut zip, "LICENSE", OPTIONAL_LIMIT)?;
        let icon = read_entry(&mut zip, "icon.png", OPTIONAL_LIMIT)?;

        Ok(PluginArchive {
            sha256,
            manifest,
            wasm,
            readme,
            license,
            icon,
        })
    }

    /// Returns the SHA-256 of the archive file.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// Returns the raw `manifest.json`.
    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// Returns the raw `plugin.wasm`.
    pub fn wasm(&self) -> &[u8] {
        &self.wasm
    }

    /// Returns the raw `README.md`, when the archive has one.
    pub fn readme(&self) -> Option<&[u8]> {
        self.readme.as_deref()
    }

    /// Returns the raw `LICENSE`, when the archive has one.
    pub fn license(&self) -> Option<&[u8]> {
        self.license.as_deref()
    }

    /// Returns the raw `icon.png`, when the archive has one.
    pub fn icon(&self) -> Option<&[u8]> {
        self.icon.as_deref()
    }
}

/// Reads one entry, refusing to allocate more than `limit` bytes.
///
/// The entry is read through a `take(limit + 1)` adapter so an entry that lies
/// about its size cannot exhaust memory.
fn read_entry(
    zip: &mut ZipArchive<Cursor<Vec<u8>>>,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, InstallError> {
    let file = match zip.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(InstallError::Archive(error)),
    };

    if file.size() > limit {
        return Err(InstallError::EntryTooLarge {
            name: name.to_owned(),
            size: file.size(),
            limit,
        });
    }

    let mut buffer = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut buffer)
        .map_err(|source| InstallError::Archive(zip::result::ZipError::Io(source)))?;

    if buffer.len() as u64 > limit {
        return Err(InstallError::EntryTooLarge {
            name: name.to_owned(),
            size: buffer.len() as u64,
            limit,
        });
    }

    Ok(Some(buffer))
}
