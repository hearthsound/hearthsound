//! The music library: scanned folders, the catalog rows they hold, retention,
//! and the browse queries behind the personal and server scopes.
//!
//! This crate owns the tables `library_folders`, `media_files`,
//! `track_retention`, `artists`, `albums`, `album_artists`, `tracks`,
//! `track_artists`, `genres`, and `track_genres`, and it owns the queries that
//! read the shared `search_fts` index. It is the only crate that writes those
//! rows, so every other crate reaches them through [`repository`].
//!
//! The repository is written against a [`sqlx::SqliteConnection`] rather than a
//! pool, so a caller that has to keep several writes atomic can pass its own
//! transaction; the [`LibraryRepository`] facade is the pool-level convenience
//! over the same functions.
//!
//! On top of the repository sit the folder scanner ([`scan`]), which indexes a
//! scanned folder's tags and keeps the rows in step with the files on disk, and
//! the cover art lookup ([`artwork`]), which answers what image belongs to a
//! local entity.
//!
//! Two rules shape the write API:
//!
//! - Hearthsound never writes into a scanned folder, so a folder row is
//!   read-only input and only `media_files` rows are created for it;
//! - retention is derived, so a pin change and the `track_retention` row it
//!   implies are written in the same transaction by [`retention`].
//!
//! Library code takes plain values and [`hearthsound_config::SettingsStore`]
//! snapshots handed to it by the composition root; it never reads the process
//! environment or a config file itself.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//!
//! use hearthsound_db::Database;
//! use hearthsound_library::LibraryRepository;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let db = Database::open(Path::new("/tmp/hearthsound.db")).await?;
//! let library = LibraryRepository::new(&db);
//! let folder = library
//!     .add_library_folder(Path::new("/music"), None)
//!     .await?;
//! println!("{} {}", folder.name, folder.path.display());
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

mod error;
mod model;
mod repository;

pub use error::LibraryError;
pub use model::{
    Album, AlbumInput, Artist, ArtistCredit, ArtistInput, Credit, Genre, LibraryFolder,
    ManagedMediaFile, MediaFile, MediaFileInput, MediaRoot, Origin, ScannedMediaFile, StorageRoots,
    Track, TrackInput, UnknownOrigin, sort_key,
};
pub use repository::LibraryRepository;

pub mod albums;
pub mod artists;
pub mod artwork;
pub mod folders;
pub mod media_files;
pub mod naming;
pub mod query;
pub mod retention;
pub mod scan;
pub mod tracks;

/// Returns the current time in unix milliseconds.
///
/// Every timestamp this crate writes shares one clock and one unit.
///
/// # Returns
///
/// The number of milliseconds since the Unix epoch.
pub(crate) fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}
