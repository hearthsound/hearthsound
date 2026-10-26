//! Reading and writing the `media_files` table.
//!
//! One row is written per playable file. A track may accumulate several files
//! across storage classes - the scanned folder copy, a permanent managed copy, an
//! evictable cache copy - and exactly one of them is marked preferred, which is
//! the file the stream service serves.
//!
//! The preferred file is chosen by storage class first and by recency second:
//!
//! 1. `library` - a permanent managed file, which eviction never touches and
//!    whose tags the import pipeline normalized;
//! 2. `folder` - a permanent file in a scanned folder, the user's own copy
//!    (written by a save or by the scanner);
//! 3. `cache` - an evictable managed file that eviction may remove.
//!
//! A permanent file is therefore never passed over for an evictable one, and a
//! managed permanent copy wins over a folder copy because it is the server's
//! canonical copy. [`refresh_preferred_file`] applies the rule after every write
//! that can change the set of a track's files.
//!
//! Every function takes a [`sqlx::SqliteConnection`] so a caller can run it
//! inside its own transaction; the pool-level convenience is on
//! [`crate::LibraryRepository`].

use std::collections::HashMap;

use hearthsound_core::{EntityId, IdPrefix, QualityTier, StorageClass};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::{
    ManagedMediaFile, MediaFile, MediaFileInput, ReplayGain, ScannedMediaFile, decode_entity,
    decode_uuid, entity_key,
};
use crate::now_millis;
use crate::query::MAX_IDS_PER_QUERY;

/// One row of the `media_files` table as it is stored.
#[derive(sqlx::FromRow)]
pub(crate) struct MediaFileRow {
    pub(crate) id: Vec<u8>,
    pub(crate) track_id: Vec<u8>,
    pub(crate) storage: String,
    pub(crate) folder_id: Option<Vec<u8>>,
    pub(crate) rel_path: String,
    pub(crate) size: i64,
    pub(crate) mtime_ms: i64,
    pub(crate) format: Option<String>,
    pub(crate) codec: Option<String>,
    pub(crate) bitrate_kbps: Option<i64>,
    pub(crate) sample_rate: Option<i64>,
    pub(crate) bit_depth: Option<i64>,
    pub(crate) channels: Option<i64>,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) lossless: Option<i64>,
    pub(crate) quality_tier: Option<String>,
    pub(crate) replaygain: Option<String>,
    pub(crate) has_embedded_art: Option<i64>,
    pub(crate) added_at: i64,
    pub(crate) last_accessed_at: Option<i64>,
    pub(crate) uploaded_by: Option<Vec<u8>>,
    pub(crate) acquisition_id: Option<Vec<u8>>,
    pub(crate) is_preferred: i64,
}

impl MediaFileRow {
    /// Converts a stored row into a media file.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::CorruptId`] when an id is not 16 bytes and
    /// [`LibraryError::UnknownLiteral`] when `storage` or `quality_tier` is
    /// unknown.
    pub(crate) fn into_media_file(self) -> Result<MediaFile, LibraryError> {
        let storage =
            self.storage
                .parse::<StorageClass>()
                .map_err(|_| LibraryError::UnknownLiteral {
                    kind: "storage class",
                    value: self.storage,
                })?;
        let quality_tier =
            match self.quality_tier {
                Some(value) => Some(value.parse::<QualityTier>().map_err(|_| {
                    LibraryError::UnknownLiteral {
                        kind: "quality tier",
                        value,
                    }
                })?),
                None => None,
            };
        Ok(MediaFile {
            id: decode_uuid(&self.id)?,
            track_id: decode_entity(IdPrefix::Track, &self.track_id)?,
            storage,
            folder_id: self.folder_id.as_deref().map(decode_uuid).transpose()?,
            rel_path: self.rel_path,
            size: self.size,
            mtime_ms: self.mtime_ms,
            format: self.format,
            codec: self.codec,
            bitrate_kbps: self.bitrate_kbps,
            sample_rate: self.sample_rate,
            bit_depth: self.bit_depth,
            channels: self.channels,
            duration_ms: self.duration_ms,
            lossless: self.lossless.map(|value| value != 0),
            quality_tier,
            replaygain: self.replaygain,
            has_embedded_art: self.has_embedded_art.map(|value| value != 0),
            added_at: self.added_at,
            last_accessed_at: self.last_accessed_at,
            uploaded_by: self.uploaded_by.as_deref().map(decode_uuid).transpose()?,
            acquisition_id: self
                .acquisition_id
                .as_deref()
                .map(decode_uuid)
                .transpose()?,
            is_preferred: self.is_preferred != 0,
        })
    }
}

/// Inserts a media file and refreshes the track's preferred file.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `file`: the file's fields.
///
/// # Returns
///
/// The new media file's id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the insert fails, including a unique
/// index violation when a row for the same storage and path already exists.
pub async fn insert_media_file(
    conn: &mut SqliteConnection,
    file: &MediaFileInput,
) -> Result<Uuid, LibraryError> {
    let id = Uuid::now_v7();
    let now = now_millis();
    let storage = file.storage.as_str();
    let folder_id = file.folder_id.map(|folder| folder.as_bytes().to_vec());
    sqlx::query!(
        "INSERT INTO media_files (id, track_id, storage, folder_id, rel_path, size, mtime_ms, \
                                  format, codec, bitrate_kbps, sample_rate, bit_depth, channels, \
                                  duration_ms, lossless, quality_tier, replaygain, \
                                  has_embedded_art, added_at, uploaded_by, acquisition_id, \
                                  is_preferred) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
        id.as_bytes().as_slice(),
        entity_key(file.track_id),
        storage,
        folder_id,
        file.rel_path,
        file.size,
        file.mtime_ms,
        file.format,
        file.codec,
        file.bitrate_kbps,
        file.sample_rate,
        file.bit_depth,
        file.channels,
        file.duration_ms,
        file.lossless,
        file.quality_tier.map(QualityTier::as_str),
        file.replaygain,
        file.has_embedded_art,
        now,
        file.uploaded_by.map(|user| user.as_bytes().to_vec()),
        file.acquisition_id
            .map(|acquisition| acquisition.as_bytes().to_vec()),
    )
    .execute(&mut *conn)
    .await?;
    refresh_preferred_file(conn, file.track_id).await?;
    Ok(id)
}

/// Inserts a managed (`library` or `cache`) media file.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `file`: the file's fields; `storage` must be `library` or `cache` and
///   `rel_path` is relative to the matching managed root.
///
/// # Returns
///
/// The new media file's id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the insert fails.
pub async fn insert_managed_media_file(
    conn: &mut SqliteConnection,
    file: &ManagedMediaFile,
) -> Result<Uuid, LibraryError> {
    insert_media_file(
        conn,
        &MediaFileInput {
            track_id: file.track_id,
            storage: file.storage,
            folder_id: None,
            rel_path: file.rel_path.clone(),
            size: file.size,
            mtime_ms: file.mtime_ms,
            format: file.format.clone(),
            codec: file.codec.clone(),
            bitrate_kbps: file.bitrate_kbps,
            sample_rate: file.sample_rate,
            bit_depth: file.bit_depth,
            channels: file.channels,
            duration_ms: file.duration_ms,
            lossless: file.lossless,
            quality_tier: file.quality_tier,
            replaygain: file.replaygain.clone(),
            has_embedded_art: file.has_embedded_art,
            uploaded_by: file.uploaded_by,
            acquisition_id: file.acquisition_id,
        },
    )
    .await
}

/// Inserts or refreshes a scanned `folder` media file.
///
/// The row is looked up by its folder and relative path. A row that exists has
/// its probe-derived columns refreshed in place, so a rescan of a file whose
/// size or modification time changed keeps the same media file id and the same
/// track. A row that does not exist is inserted.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track the file belongs to.
/// - `file`: the scanned file's fields.
///
/// # Returns
///
/// The media file's id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub async fn upsert_folder_media_file(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    file: &ScannedMediaFile,
) -> Result<Uuid, LibraryError> {
    let existing: Option<Vec<u8>> = sqlx::query_scalar!(
        "SELECT id FROM media_files WHERE storage = 'folder' AND folder_id = ? AND rel_path = ?",
        file.folder_id.as_bytes().as_slice(),
        file.rel_path,
    )
    .fetch_optional(&mut *conn)
    .await?;

    let id = match existing {
        Some(bytes) => {
            let id = decode_uuid(&bytes)?;
            let replaygain = file.replaygain.as_ref().and_then(ReplayGain::to_json);
            sqlx::query!(
                "UPDATE media_files SET size = ?, mtime_ms = ?, format = ?, codec = ?, \
                        bitrate_kbps = ?, sample_rate = ?, bit_depth = ?, channels = ?, \
                        duration_ms = ?, lossless = ?, replaygain = ?, has_embedded_art = ? \
                        WHERE id = ?",
                file.size,
                file.mtime_ms,
                file.format,
                file.codec,
                file.bitrate_kbps,
                file.sample_rate,
                file.bit_depth,
                file.channels,
                file.duration_ms,
                file.lossless,
                replaygain,
                file.has_embedded_art,
                id.as_bytes().as_slice(),
            )
            .execute(&mut *conn)
            .await?;
            id
        }
        None => {
            let id = Uuid::now_v7();
            let now = now_millis();
            let replaygain = file.replaygain.as_ref().and_then(ReplayGain::to_json);
            sqlx::query!(
                "INSERT INTO media_files (id, track_id, storage, folder_id, rel_path, size, \
                                          mtime_ms, format, codec, bitrate_kbps, sample_rate, \
                                          bit_depth, channels, duration_ms, lossless, replaygain, \
                                          has_embedded_art, added_at, is_preferred) \
                 VALUES (?, ?, 'folder', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
                id.as_bytes().as_slice(),
                entity_key(track_id),
                file.folder_id.as_bytes().as_slice(),
                file.rel_path,
                file.size,
                file.mtime_ms,
                file.format,
                file.codec,
                file.bitrate_kbps,
                file.sample_rate,
                file.bit_depth,
                file.channels,
                file.duration_ms,
                file.lossless,
                replaygain,
                file.has_embedded_art,
                now,
            )
            .execute(&mut *conn)
            .await?;
            id
        }
    };

    refresh_preferred_file(conn, track_id).await?;
    Ok(id)
}

/// Marks the file the stream service serves for a track.
///
/// All of the track's files are cleared and the winner of the storage-class
/// ordering documented on this module is marked preferred, with the most
/// recently added file winning a tie. A track with no files is left with no
/// preferred row.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track whose files changed.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub async fn refresh_preferred_file(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<(), LibraryError> {
    sqlx::query!(
        "UPDATE media_files SET is_preferred = 0 WHERE track_id = ? AND is_preferred = 1",
        entity_key(track_id),
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "UPDATE media_files SET is_preferred = 1 WHERE id = ( \
             SELECT id FROM media_files WHERE track_id = ? \
             ORDER BY CASE storage WHEN 'library' THEN 0 WHEN 'folder' THEN 1 ELSE 2 END, \
                      added_at DESC, id LIMIT 1)",
        entity_key(track_id),
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Reads a media file by its id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the media file's id.
///
/// # Returns
///
/// The media file, or `None` when no row has that id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn get_media_file(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> Result<Option<MediaFile>, LibraryError> {
    let row = sqlx::query_as!(
        MediaFileRow,
        "SELECT id, track_id, storage, folder_id, rel_path, size, mtime_ms, format, codec, \
                bitrate_kbps, sample_rate, bit_depth, channels, duration_ms, lossless, \
                quality_tier, replaygain, has_embedded_art, added_at, last_accessed_at, \
                uploaded_by, acquisition_id, is_preferred FROM media_files WHERE id = ?",
        id.as_bytes().as_slice(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(MediaFileRow::into_media_file).transpose()
}

/// Reads the `folder` media file at a folder and relative path.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `folder_id`: the library folder's id.
/// - `rel_path`: the path relative to the folder root.
///
/// # Returns
///
/// The media file, or `None` when no row has that path.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn find_folder_media_file(
    conn: &mut SqliteConnection,
    folder_id: Uuid,
    rel_path: &str,
) -> Result<Option<MediaFile>, LibraryError> {
    let row = sqlx::query_as!(
        MediaFileRow,
        "SELECT id, track_id, storage, folder_id, rel_path, size, mtime_ms, format, codec, \
                bitrate_kbps, sample_rate, bit_depth, channels, duration_ms, lossless, \
                quality_tier, replaygain, has_embedded_art, added_at, last_accessed_at, \
                uploaded_by, acquisition_id, is_preferred FROM media_files \
         WHERE storage = 'folder' AND folder_id = ? AND rel_path = ?",
        folder_id.as_bytes().as_slice(),
        rel_path,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(MediaFileRow::into_media_file).transpose()
}

/// Lists a track's media files, preferred first.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track's client id.
///
/// # Returns
///
/// The track's files, the preferred one first, then the rest by storage class
/// and path.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_media_files_for_track(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Vec<MediaFile>, LibraryError> {
    let rows = sqlx::query_as!(
        MediaFileRow,
        "SELECT id, track_id, storage, folder_id, rel_path, size, mtime_ms, format, codec, \
                bitrate_kbps, sample_rate, bit_depth, channels, duration_ms, lossless, \
                quality_tier, replaygain, has_embedded_art, added_at, last_accessed_at, \
                uploaded_by, acquisition_id, is_preferred FROM media_files WHERE track_id = ? \
         ORDER BY is_preferred DESC, storage, rel_path, id",
        entity_key(track_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(MediaFileRow::into_media_file)
        .collect()
}

/// Reads the track's preferred media file.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track's client id.
///
/// # Returns
///
/// The preferred file, or `None` when the track has no files.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn preferred_media_file(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Option<MediaFile>, LibraryError> {
    let row = sqlx::query_as!(
        MediaFileRow,
        "SELECT id, track_id, storage, folder_id, rel_path, size, mtime_ms, format, codec, \
                bitrate_kbps, sample_rate, bit_depth, channels, duration_ms, lossless, \
                quality_tier, replaygain, has_embedded_art, added_at, last_accessed_at, \
                uploaded_by, acquisition_id, is_preferred FROM media_files \
         WHERE track_id = ? AND is_preferred = 1 LIMIT 1",
        entity_key(track_id),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(MediaFileRow::into_media_file).transpose()
}

/// Reads the preferred media file of each of the given tracks.
///
/// This is the batch form of [`preferred_media_file`]: it selects the same row
/// per track, with the ids read in chunks of [`MAX_IDS_PER_QUERY`].
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_ids`: the tracks to read, in any order.
///
/// # Returns
///
/// The preferred file of each track that has one, keyed by track; a track with
/// no files is absent.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails or a stored row does not decode.
pub async fn preferred_media_files(
    conn: &mut SqliteConnection,
    track_ids: &[EntityId],
) -> Result<HashMap<EntityId, MediaFile>, LibraryError> {
    let mut files: HashMap<EntityId, MediaFile> = HashMap::new();
    for chunk in track_ids.chunks(MAX_IDS_PER_QUERY) {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT id, track_id, storage, folder_id, rel_path, size, mtime_ms, format, codec, \
                    bitrate_kbps, sample_rate, bit_depth, channels, duration_ms, lossless, \
                    quality_tier, replaygain, has_embedded_art, added_at, last_accessed_at, \
                    uploaded_by, acquisition_id, is_preferred FROM media_files \
             WHERE is_preferred = 1 AND track_id IN (",
        );
        let mut separated = builder.separated(", ");
        for id in chunk {
            separated.push_bind(entity_key(*id));
        }
        separated.push_unseparated(")");
        let rows = builder
            .build_query_as::<MediaFileRow>()
            .fetch_all(&mut *conn)
            .await?;
        for row in rows {
            let file = row.into_media_file()?;
            files.entry(file.track_id).or_insert(file);
        }
    }
    Ok(files)
}

/// The stored state of one media file of a folder.
///
/// A scan compares a walked file against this: the size and modification time
/// decide whether the file needs probing, and the id and track let the write
/// refresh the row and its track instead of inserting a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredFileState {
    /// The media file's id.
    pub id: Uuid,
    /// The track the file belongs to.
    pub track_id: EntityId,
    /// The stored file size in bytes.
    pub size: i64,
    /// The stored file modification time in unix milliseconds.
    pub mtime_ms: i64,
}

/// Lists the stored state of every media file of a folder.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `folder_id`: the library folder's id.
///
/// # Returns
///
/// The folder's files, keyed by their path relative to the folder root.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_folder_media_files(
    conn: &mut SqliteConnection,
    folder_id: Uuid,
) -> Result<HashMap<String, StoredFileState>, LibraryError> {
    let rows = sqlx::query!(
        "SELECT id, track_id, rel_path, size, mtime_ms FROM media_files \
         WHERE storage = 'folder' AND folder_id = ?",
        folder_id.as_bytes().as_slice(),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.rel_path,
                StoredFileState {
                    id: decode_uuid(&row.id)?,
                    track_id: decode_entity(IdPrefix::Track, &row.track_id)?,
                    size: row.size,
                    mtime_ms: row.mtime_ms,
                },
            ))
        })
        .collect()
}

/// Lists the stored state of the media files of a folder under one of its
/// directories.
///
/// A folder's scan uses this to find the rows an enclosing folder holds for the
/// files it is about to index: a file belongs to the deepest folder that
/// contains it, so the deeper folder re-points those rows instead of indexing
/// the same file a second time.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `folder_id`: the library folder's id.
/// - `prefix`: the directory, relative to the folder root and without a
///   trailing `/`.
///
/// # Returns
///
/// The rows under `prefix`, keyed by their path relative to the folder root.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_folder_media_files_under(
    conn: &mut SqliteConnection,
    folder_id: Uuid,
    prefix: &str,
) -> Result<HashMap<String, StoredFileState>, LibraryError> {
    // `/` sorts directly before `0`, so the range covers every path below the
    // directory and nothing beside it, on the index the folder and path share.
    let lower = format!("{prefix}/");
    let upper = format!("{prefix}0");
    let rows = sqlx::query!(
        "SELECT id, track_id, rel_path, size, mtime_ms FROM media_files \
         WHERE storage = 'folder' AND folder_id = ? AND rel_path >= ? AND rel_path < ?",
        folder_id.as_bytes().as_slice(),
        lower,
        upper,
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.rel_path,
                StoredFileState {
                    id: decode_uuid(&row.id)?,
                    track_id: decode_entity(IdPrefix::Track, &row.track_id)?,
                    size: row.size,
                    mtime_ms: row.mtime_ms,
                },
            ))
        })
        .collect()
}

/// Moves a `folder` media file to another folder and relative path.
///
/// This is what records that a file a folder indexes is owned by a folder
/// inside it: the row keeps its id and its track, so the track's likes, saves,
/// ratings, playlist entries and plays survive the move, and only the folder and
/// the path change. The track's preferred file does not change, because the
/// storage class does not.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the media file that moved.
/// - `folder_id`: the folder that owns the file now.
/// - `rel_path`: its path relative to that folder's root.
///
/// # Returns
///
/// `true` when the row was updated, `false` when no `folder` row has that id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails, including a unique
/// index violation when a row already holds the same folder and path.
pub async fn repoint_folder_media_file(
    conn: &mut SqliteConnection,
    id: Uuid,
    folder_id: Uuid,
    rel_path: &str,
) -> Result<bool, LibraryError> {
    let affected = sqlx::query!(
        "UPDATE media_files SET folder_id = ?, rel_path = ? WHERE id = ? AND storage = 'folder'",
        folder_id.as_bytes().as_slice(),
        rel_path,
        id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Moves a managed media file to another storage class and relative path.
///
/// This is what records the new home of a file the storage maintainer moved
/// between the two managed roots: the columns change, the track's preferred
/// file is refreshed, because a `library` file outranks a `folder` file and a
/// `cache` file does not, and the track's availability is left to the caller,
/// which knows which pins apply.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the media file that moved.
/// - `storage`: the root the file is in now.
/// - `rel_path`: its path relative to that root.
///
/// # Returns
///
/// `true` when the row was updated, `false` when no row has that id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails, including a unique
/// index violation when a row already holds the same storage and path.
pub async fn move_managed_file(
    conn: &mut SqliteConnection,
    id: Uuid,
    storage: StorageClass,
    rel_path: &str,
) -> Result<bool, LibraryError> {
    let moved: Option<Vec<u8>> = sqlx::query_scalar!(
        "UPDATE media_files SET storage = ?, rel_path = ? WHERE id = ? RETURNING track_id",
        storage.as_str(),
        rel_path,
        id.as_bytes().as_slice(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    match moved {
        Some(bytes) => {
            let track_id = decode_entity(IdPrefix::Track, &bytes)?;
            refresh_preferred_file(conn, track_id).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Deletes a media file and refreshes the track's preferred file.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the media file to delete.
///
/// # Returns
///
/// `true` when a row was deleted, `false` when the file does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub async fn delete_media_file(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> Result<bool, LibraryError> {
    let deleted: Option<Vec<u8>> = sqlx::query_scalar!(
        "DELETE FROM media_files WHERE id = ? RETURNING track_id",
        id.as_bytes().as_slice(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    match deleted {
        Some(bytes) => {
            let track_id = decode_entity(IdPrefix::Track, &bytes)?;
            refresh_preferred_file(conn, track_id).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}
