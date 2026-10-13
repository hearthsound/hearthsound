//! The [`Layer`] enum and its string conversion.

use std::fmt;
use std::str::FromStr;

/// A pluggable behavior of the server.
///
/// Every layer has exactly one core provider trait (see the `providers` module
/// of this crate); the plugin host adapts each plugin's WebAssembly exports to
/// the matching trait. The nine variants are fixed by the architecture: they
/// are the set of behaviors a plugin can provide.
///
/// # Examples
///
/// ```
/// use hearthsound_core::Layer;
///
/// assert_eq!(Layer::Catalog.as_str(), "catalog");
/// assert_eq!("catalog".parse::<Layer>().unwrap(), Layer::Catalog);
/// assert!("Catalog".parse::<Layer>().is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Layer {
    /// Catalog search and entity lookup.
    Catalog,
    /// Track and album metadata enrichment.
    Metadata,
    /// Cover art and artist image lookup.
    Artwork,
    /// Acquiring missing content through an external downloader.
    Acquisition,
    /// Verifying that acquired audio matches what was requested.
    Verification,
    /// Reporting plays to a scrobbling service.
    Scrobble,
    /// Recommendations and similar-artist discovery.
    Discovery,
    /// Lyrics lookup.
    Lyrics,
    /// Importing playlists from an external service.
    PlaylistImport,
}

impl Layer {
    /// Every layer, in a fixed order.
    ///
    /// The order matches the variant declaration order and is stable across
    /// releases.
    pub const ALL: [Layer; 9] = [
        Layer::Catalog,
        Layer::Metadata,
        Layer::Artwork,
        Layer::Acquisition,
        Layer::Verification,
        Layer::Scrobble,
        Layer::Discovery,
        Layer::Lyrics,
        Layer::PlaylistImport,
    ];

    /// Returns the stable wire name of this layer.
    ///
    /// The name is the lowercase form used in the plugin manifest, the WIT
    /// interface name, and the database.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::Layer;
    ///
    /// assert_eq!(Layer::PlaylistImport.as_str(), "playlist-import");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            Layer::Catalog => "catalog",
            Layer::Metadata => "metadata",
            Layer::Artwork => "artwork",
            Layer::Acquisition => "acquisition",
            Layer::Verification => "verification",
            Layer::Scrobble => "scrobble",
            Layer::Discovery => "discovery",
            Layer::Lyrics => "lyrics",
            Layer::PlaylistImport => "playlist-import",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`Layer`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown layer `{0}`")]
pub struct UnknownLayer(pub String);

impl FromStr for Layer {
    type Err = UnknownLayer;

    /// Parses a layer from its wire name.
    ///
    /// The comparison is exact and case-sensitive.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownLayer`] when `s` is not one of the names produced by
    /// [`Layer::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|layer| layer.as_str() == s)
            .ok_or_else(|| UnknownLayer(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_layer_round_trips() {
        for layer in Layer::ALL {
            assert_eq!(layer.as_str().parse::<Layer>().unwrap(), layer);
            assert_eq!(layer.to_string(), layer.as_str());
        }
    }

    #[test]
    fn names_are_unique() {
        let mut names: Vec<&str> = Layer::ALL.iter().map(|layer| layer.as_str()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn rejects_unknown_and_wrong_case() {
        assert_eq!(
            "Catalog".parse::<Layer>(),
            Err(UnknownLayer("Catalog".to_owned()))
        );
        assert!("nope".parse::<Layer>().is_err());
    }
}
