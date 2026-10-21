//! The pool-level convenience over the library write and read functions.
//!
//! [`LibraryRepository`] borrows the database's writer and reader pools and
//! offers one method per operation. A write opens a short transaction, calls the
//! matching function in [`crate::artists`], [`crate::albums`],
//! [`crate::tracks`], [`crate::media_files`], or [`crate::folders`], and
//! commits; a read borrows a reader connection.
//!
//! A caller that has to keep several writes atomic - the import of an acquired
//! file, or a folder scan - must not use these methods, because each one commits
//! on its own. Such a caller opens a transaction and calls the free functions
//! with it instead.

use std::collections::HashMap;
use std::path::Path;

use hearthsound_core::{Availability, EntityId, PinReason};
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::media_files::StoredFileState;
use crate::model::{
    Album, AlbumInput, Artist, ArtistCredit, ArtistInput, Credit, Genre, LibraryFolder,
    ManagedMediaFile, MediaFile, MediaFileInput, Origin, ScannedMediaFile, Track, TrackInput,
};
use crate::{READ_THEN_WRITE, albums, artists, folders, media_files, retention, tracks};

/// The library tables, read and written over a database's two pools.
///
/// # Examples
///
/// ```no_run
/// use hearthsound_library::{ArtistInput, LibraryRepository};
///
/// # async fn example(db: &hearthsound_db::Database) -> Result<(), Box<dyn std::error::Error>> {
/// let library = LibraryRepository::new(db);
/// let id = library.upsert_artist(&ArtistInput::new("Energy 52")).await?;
/// assert_eq!(library.get_artist(id).await?.unwrap().name, "Energy 52");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct LibraryRepository {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

impl LibraryRepository {
    /// Builds the repository over a database.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    ///
    /// # Returns
    ///
    /// The repository.
    pub fn new(db: &Database) -> LibraryRepository {
        LibraryRepository {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Inserts or refreshes an artist in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn upsert_artist(&self, artist: &ArtistInput) -> Result<EntityId, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let id = artists::upsert_artist(&mut tx, artist).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Inserts or refreshes an album and its credits in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn upsert_album(
        &self,
        album: &AlbumInput,
        credits: &[ArtistCredit],
    ) -> Result<EntityId, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let id = albums::upsert_album(&mut tx, album, credits).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Inserts a track with its credits and genres in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn insert_track(
        &self,
        track: &TrackInput,
        credits: &[ArtistCredit],
        genres: &[String],
    ) -> Result<EntityId, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let id = tracks::insert_track(&mut tx, track, credits, genres).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Refreshes a track's fields, credits, and genres in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn update_track(
        &self,
        track_id: EntityId,
        track: &TrackInput,
        credits: &[ArtistCredit],
        genres: &[String],
    ) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let updated = tracks::update_track(&mut tx, track_id, track, credits, genres).await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Inserts a media file in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn insert_media_file(&self, file: &MediaFileInput) -> Result<Uuid, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let id = media_files::insert_media_file(&mut tx, file).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Inserts a managed media file in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn insert_managed_media_file(
        &self,
        file: &ManagedMediaFile,
    ) -> Result<Uuid, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let id = media_files::insert_managed_media_file(&mut tx, file).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Inserts or refreshes a scanned folder media file in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn upsert_folder_media_file(
        &self,
        track_id: EntityId,
        file: &ScannedMediaFile,
    ) -> Result<Uuid, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let id = media_files::upsert_folder_media_file(&mut tx, track_id, file).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Deletes a media file in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn delete_media_file(&self, id: Uuid) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let deleted = media_files::delete_media_file(&mut tx, id).await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// Adds a library folder in its own transaction.
    ///
    /// A folder that already exists is reported rather than duplicated, so the
    /// transaction reads before it writes and starts as a write; see
    /// [`crate::READ_THEN_WRITE`].
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the path is unusable or the write fails.
    pub async fn add_library_folder(
        &self,
        path: &Path,
        owner_user_id: Option<Uuid>,
    ) -> Result<LibraryFolder, LibraryError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        let folder = folders::add_library_folder(&mut tx, path, owner_user_id).await?;
        tx.commit().await?;
        Ok(folder)
    }

    /// Removes a library folder, its file records and the tracks only it held,
    /// in its own transaction.
    ///
    /// A track that still has a file in the library or the cache keeps its row;
    /// a local track whose last file this folder held is deleted, exactly as the
    /// scanner's pruning does, and the caller publishes the change. Nothing
    /// inside the folder on disk is removed or moved.
    ///
    /// # Parameters
    ///
    /// - `id`: the folder's UUIDv7 primary key.
    ///
    /// # Returns
    ///
    /// The tracks that were deleted, so the caller can announce them; an empty
    /// list when the folder held nothing or no longer exists.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn remove_library_folder(&self, id: Uuid) -> Result<Vec<EntityId>, LibraryError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        // The folder's files stop pinning their tracks before they go.
        retention::repin_folder_tracks(&mut tx, id, false).await?;
        let files = media_files::list_folder_media_files(&mut tx, id).await?;
        let mut removed: Vec<EntityId> = Vec::new();
        for state in files.values() {
            media_files::delete_media_file(&mut tx, state.id).await?;
            let remaining =
                media_files::list_media_files_for_track(&mut tx, state.track_id).await?;
            if !remaining.is_empty() {
                continue;
            }
            match tracks::get_track(&mut tx, state.track_id).await? {
                Some(track) if track.origin == Origin::Local => {
                    tracks::delete_track(&mut tx, state.track_id).await?;
                    removed.push(state.track_id);
                }
                Some(_) => {
                    tracks::set_track_availability(
                        &mut tx,
                        state.track_id,
                        Availability::Remote,
                        None,
                    )
                    .await?;
                }
                None => {}
            }
        }
        folders::delete_library_folder(&mut tx, id).await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// Renames a library folder in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn rename_library_folder(&self, id: Uuid, name: &str) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let renamed = folders::rename_library_folder(&mut tx, id, name).await?;
        tx.commit().await?;
        Ok(renamed)
    }

    /// Associates a library folder with a user, or makes it generic, in its own
    /// transaction.
    ///
    /// The folder's tracks are re-pinned by the retention layer inside the same
    /// transaction, so the `folder` reason and the availability never lag the
    /// ownership column.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn set_library_folder_owner(
        &self,
        id: Uuid,
        owner_user_id: Option<Uuid>,
    ) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let updated = retention::set_folder_owner(&mut tx, id, owner_user_id).await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Adds a pin reason to a track in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn add_pin_reason(
        &self,
        track_id: EntityId,
        reason: PinReason,
    ) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let added = retention::add_pin_reason(&mut tx, track_id, reason).await?;
        tx.commit().await?;
        Ok(added)
    }

    /// Removes a pin reason from a track in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn remove_pin_reason(
        &self,
        track_id: EntityId,
        reason: PinReason,
    ) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let removed = retention::remove_pin_reason(&mut tx, track_id, reason).await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// Reports whether a track is pinned.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn track_is_pinned(&self, track_id: EntityId) -> Result<bool, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        retention::track_is_pinned(&mut conn, track_id).await
    }

    /// Reads an artist.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn get_artist(&self, id: EntityId) -> Result<Option<Artist>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        artists::get_artist(&mut conn, id).await
    }

    /// Reads an artist by its MusicBrainz artist id.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn find_artist_by_mb_id(&self, mbid: &str) -> Result<Option<Artist>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        artists::find_artist_by_mb_id(&mut conn, mbid).await
    }

    /// Lists every artist.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_artists(&self) -> Result<Vec<Artist>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        artists::list_artists(&mut conn).await
    }

    /// Reads an album.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn get_album(&self, id: EntityId) -> Result<Option<Album>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        albums::get_album(&mut conn, id).await
    }

    /// Lists the albums a primary artist is credited on.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_albums_for_artist(
        &self,
        artist_id: EntityId,
    ) -> Result<Vec<Album>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        albums::list_albums_for_artist(&mut conn, artist_id).await
    }

    /// Lists an album's artist credits.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_album_credits(
        &self,
        album_id: EntityId,
    ) -> Result<Vec<Credit>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        albums::list_album_credits(&mut conn, album_id).await
    }

    /// Reads a track.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn get_track(&self, id: EntityId) -> Result<Option<Track>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        tracks::get_track(&mut conn, id).await
    }

    /// Lists an album's tracks in playing order.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_tracks_for_album(
        &self,
        album_id: EntityId,
    ) -> Result<Vec<Track>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        tracks::list_tracks_for_album(&mut conn, album_id).await
    }

    /// Lists a track's artist credits.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_track_credits(
        &self,
        track_id: EntityId,
    ) -> Result<Vec<Credit>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        tracks::list_track_credits(&mut conn, track_id).await
    }

    /// Lists a track's genres.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_track_genres(&self, track_id: EntityId) -> Result<Vec<Genre>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        tracks::list_track_genres(&mut conn, track_id).await
    }

    /// Lists the artists credited on a track.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_track_artists(
        &self,
        track_id: EntityId,
    ) -> Result<Vec<Artist>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        artists::list_track_artists(&mut conn, track_id).await
    }

    /// Lists the artists credited on an album.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_album_artists(
        &self,
        album_id: EntityId,
    ) -> Result<Vec<Artist>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        artists::list_album_artists(&mut conn, album_id).await
    }

    /// Writes a track's availability in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn set_track_availability(
        &self,
        track_id: EntityId,
        availability: Availability,
        unavailable_until: Option<i64>,
    ) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let updated =
            tracks::set_track_availability(&mut tx, track_id, availability, unavailable_until)
                .await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Reads a media file.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn get_media_file(&self, id: Uuid) -> Result<Option<MediaFile>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        media_files::get_media_file(&mut conn, id).await
    }

    /// Reads the `folder` media file at a folder and relative path.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn find_folder_media_file(
        &self,
        folder_id: Uuid,
        rel_path: &str,
    ) -> Result<Option<MediaFile>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        media_files::find_folder_media_file(&mut conn, folder_id, rel_path).await
    }

    /// Lists a track's media files, preferred first.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_media_files_for_track(
        &self,
        track_id: EntityId,
    ) -> Result<Vec<MediaFile>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        media_files::list_media_files_for_track(&mut conn, track_id).await
    }

    /// Reads the track's preferred media file.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn preferred_media_file(
        &self,
        track_id: EntityId,
    ) -> Result<Option<MediaFile>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        media_files::preferred_media_file(&mut conn, track_id).await
    }

    /// Lists the stored state of every media file of a folder, keyed by its
    /// path relative to the folder root.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_folder_media_files(
        &self,
        folder_id: Uuid,
    ) -> Result<HashMap<String, StoredFileState>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        media_files::list_folder_media_files(&mut conn, folder_id).await
    }

    /// Records whether a library folder is watched for filesystem changes, in
    /// its own transaction.
    ///
    /// The running server picks the change up on its next reload; a stop while
    /// the folder was watched drops its watcher then, and a start begins one.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    pub async fn set_library_folder_watch(
        &self,
        id: Uuid,
        watch: bool,
    ) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin().await?;
        let updated = folders::set_library_folder_watch(&mut tx, id, watch).await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Reads a library folder.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn get_library_folder(
        &self,
        id: Uuid,
    ) -> Result<Option<LibraryFolder>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        folders::get_library_folder(&mut conn, id).await
    }

    /// Lists every library folder.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the read fails.
    pub async fn list_library_folders(&self) -> Result<Vec<LibraryFolder>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        folders::list_library_folders(&mut conn).await
    }
}
