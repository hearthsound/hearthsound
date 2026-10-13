//! Creating the on-disk layout the server owns.
//!
//! The layout is the one `02-architecture.md` section 12 fixes: a data tree
//! holding the database, the secret key, the installed plugins, the artwork
//! cache, and the pre-migration backups, and a storage tree holding the managed
//! library, the evictable cache, the per-acquisition staging directories, and
//! the upload landing area. Every directory is derived from `paths.data` and
//! `paths.storage` by the bootstrap configuration, so this module only creates
//! what [`BootstrapConfig`] already resolved.

use std::path::Path;

use hearthsound_config::bootstrap::BootstrapConfig;

use crate::error::ServerError;

/// Creates every directory of the data and storage trees.
///
/// Creating a directory that exists is not an error, so this is safe to run on
/// every start and on every command-line command that opens the data directory.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
///
/// # Returns
///
/// `Ok(())` once every directory exists.
///
/// # Errors
///
/// Returns [`ServerError::Directory`] with the failing path when a directory
/// cannot be created.
///
/// # Examples
///
/// ```no_run
/// use std::collections::BTreeMap;
///
/// use hearthsound_config::bootstrap::BootstrapConfig;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = BootstrapConfig::load(None, &BTreeMap::new())?;
/// hearthsound_server::paths::prepare(&config)?;
/// # Ok(())
/// # }
/// ```
pub fn prepare(config: &BootstrapConfig) -> Result<(), ServerError> {
    for path in directories(config) {
        create(path)?;
    }
    Ok(())
}

/// The directories of the layout, in creation order.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
///
/// # Returns
///
/// The data directory, the storage tree, and the directories derived from them.
pub fn directories(config: &BootstrapConfig) -> Vec<&Path> {
    vec![
        config.paths.data.as_path(),
        config.paths.storage.as_path(),
        config.paths.plugins.as_path(),
        config.paths.artwork.as_path(),
        config.paths.backups.as_path(),
        config.paths.library.as_path(),
        config.paths.cache.as_path(),
        config.paths.staging.as_path(),
        config.paths.uploads_staging.as_path(),
    ]
}

/// Creates one directory and its parents.
///
/// # Parameters
///
/// - `path`: the directory to create.
///
/// # Errors
///
/// Returns [`ServerError::Directory`] when the directory cannot be created.
fn create(path: &Path) -> Result<(), ServerError> {
    std::fs::create_dir_all(path).map_err(|source| ServerError::Directory {
        path: path.to_path_buf(),
        source,
    })
}
