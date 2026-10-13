//! The [`EntityKind`] enum.

use serde::{Deserialize, Serialize};

/// The kind of catalog entity a query or result refers to.
///
/// `EntityKind` is used both to restrict a catalog search and to describe what
/// an index entry points at. Its literals are the `entity-kind` variants of the
/// plugin interface, written in lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    /// A track.
    Track,
    /// An album.
    Album,
    /// An artist.
    Artist,
    /// A playlist.
    Playlist,
}
