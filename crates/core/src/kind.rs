//! The [`EntityKind`] enum.

/// The kind of catalog entity a query or result refers to.
///
/// `EntityKind` is used both to restrict a catalog search and to describe what
/// an index entry points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
