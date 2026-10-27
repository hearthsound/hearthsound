//! The plugins the server installs by itself and will not let an administrator
//! remove.
//!
//! A mandatory plugin, such as the MusicBrainz stack the entity index is built
//! on, is part of the product: the server installs it at startup and refuses the
//! writes that would leave it gone, switched off, or absent from a layer its
//! manifest provides. Administrators may still reorder a layer it takes part in.
//!
//! The set is data, not a constant compiled into the plugin host: the server
//! passes the ids `bundled-plugins.lock` marks `mandatory = true`, and a test
//! passes whatever ids it needs.

use std::collections::BTreeSet;

/// The plugin ids the server refuses to let an administrator remove or disable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtectedPlugins {
    ids: BTreeSet<String>,
}

impl ProtectedPlugins {
    /// Builds a set from plugin ids.
    ///
    /// # Parameters
    ///
    /// - `ids`: the plugin ids, in any order; duplicates collapse.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_plugin_host::ProtectedPlugins;
    ///
    /// let protected = ProtectedPlugins::new(["app.hearthsound.musicbrainz"]);
    /// assert!(protected.contains("app.hearthsound.musicbrainz"));
    /// assert!(!protected.contains("app.hearthsound.deezer"));
    /// ```
    pub fn new<I, S>(ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            ids: ids.into_iter().map(Into::into).collect(),
        }
    }

    /// Builds an empty set, for a caller that protects nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// Returns whether `plugin_id` is protected.
    pub fn contains(&self, plugin_id: &str) -> bool {
        self.ids.contains(plugin_id)
    }

    /// Returns whether no plugin is protected.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Returns the protected ids, in sorted order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.ids.iter().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_collapses_duplicates_and_reports_membership() {
        let protected = ProtectedPlugins::new([
            "app.hearthsound.musicbrainz",
            "app.hearthsound.musicbrainz",
            "app.hearthsound.deezer",
        ]);
        assert_eq!(
            protected.ids().collect::<Vec<_>>(),
            ["app.hearthsound.deezer", "app.hearthsound.musicbrainz"]
        );
        assert!(protected.contains("app.hearthsound.deezer"));
        assert!(!protected.contains("app.hearthsound.slskd"));
        assert!(!ProtectedPlugins::none().contains("app.hearthsound.deezer"));
        assert!(ProtectedPlugins::none().is_empty());
    }
}
