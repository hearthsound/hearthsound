//! The plugins a Hearthsound server enables by default.
//!
//! `bundled-plugins.lock` at the repository root is the one place that names
//! the first-party plugins the server ships with. A build reads it at compile
//! time and exposes the ids here, so the first-run wizard can preselect exactly
//! the plugins the release pins instead of a list that could drift from it.
//!
//! Only the ids are read; the versions and digests in the lock file belong to
//! `cargo xtask fetch-plugins`, which downloads the archives.

use std::sync::LazyLock;

use serde::Deserialize;

/// The contents of `bundled-plugins.lock`.
#[derive(Debug, Deserialize)]
struct Lock {
    /// One entry per bundled archive. A lock file without the table is empty
    /// rather than an error, so a repository without pins still builds.
    #[serde(default)]
    plugin: Vec<LockEntry>,
}

/// One archive pinned in `bundled-plugins.lock`.
#[derive(Debug, Deserialize)]
struct LockEntry {
    /// The plugin id from its manifest.
    id: String,
}

/// The pinned ids, parsed once on first use.
static DEFAULT_PLUGIN_IDS: LazyLock<Vec<String>> = LazyLock::new(|| {
    let raw = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../bundled-plugins.lock"
    ));
    let lock: Lock = match toml::from_str(raw) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::error!(%error, "could not read the bundled plugin lock file");
            return Vec::new();
        }
    };
    lock.plugin.into_iter().map(|entry| entry.id).collect()
});

/// Returns the ids of the plugins the server ships with.
///
/// The order is the lock file's. The slice is empty when the lock file cannot
/// be read, which the log says and which leaves a caller with nothing to
/// preselect.
///
/// # Returns
///
/// The pinned plugin ids, in lock file order.
///
/// # Examples
///
/// ```
/// let ids = hearthsound_plugin_host::default_plugin_ids();
/// assert!(ids.contains(&"app.hearthsound.deezer".to_owned()));
/// ```
pub fn default_plugin_ids() -> &'static [String] {
    &DEFAULT_PLUGIN_IDS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_file_pins_the_first_party_plugins() {
        let ids = default_plugin_ids();
        for id in [
            "app.hearthsound.deezer",
            "app.hearthsound.listenbrainz",
            "app.hearthsound.musicbrainz",
            "app.hearthsound.slskd",
        ] {
            assert!(ids.contains(&id.to_owned()), "{id} is pinned: {ids:?}");
        }
        assert_eq!(ids.len(), 4, "the lock file pins four plugins: {ids:?}");
    }
}
