//! Whether the library holds an entity.
//!
//! "Held" is the one rule every page, the indexer and the image service share:
//! a track the server stores a file for or that a user saved, an album a user
//! saved or that holds such a track, and an artist a user saved or that is
//! credited on such an album or track. A held entity is shown from stored data
//! alone - its page is queued for the indexer instead of being fetched while it
//! is viewed - and only a remote entity may cause a provider call on the request
//! path.
//!
//! A playlist is held when it has entries, which is a question the playlist
//! reader answers itself; this module reports `false` for one.

use hearthsound_core::EntityKind;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::CatalogError;

/// The track rule: a stored file, in any storage, or a save.
///
/// A cached file counts: eviction is the retention pass's business, and a
/// library track whose file was evicted is still the library's track.
const TRACK_HELD: &str = "SELECT 1 AS \"found: i64\" FROM tracks t WHERE t.id = ? AND ( \
     EXISTS (SELECT 1 FROM media_files mf WHERE mf.track_id = t.id) \
     OR EXISTS (SELECT 1 FROM user_library ul \
                WHERE ul.entity_kind = 'track' AND ul.entity_id = t.id))";

/// Reports whether the library holds an entity.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `id`: the row's raw primary key.
///
/// # Returns
///
/// `true` when the library holds the entity.
///
/// # Errors
///
/// Returns [`CatalogError`] when the read fails.
pub(crate) async fn is_held(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    id: Uuid,
) -> Result<bool, CatalogError> {
    let found: Option<i64> = match kind {
        EntityKind::Track => {
            sqlx::query_scalar(TRACK_HELD)
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?
        }
        EntityKind::Album => {
            sqlx::query_scalar(ALBUM_HELD)
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?
        }
        EntityKind::Artist => {
            sqlx::query_scalar(ARTIST_HELD)
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?
        }
        EntityKind::Playlist => None,
    };
    Ok(found.is_some())
}

/// Reports whether the library holds an album.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the album's raw primary key.
///
/// # Returns
///
/// `true` when a user saved the album or the album holds a held track.
///
/// # Errors
///
/// Returns [`CatalogError`] when the read fails.
pub(crate) async fn album(conn: &mut SqliteConnection, id: Uuid) -> Result<bool, CatalogError> {
    let found: Option<i64> = sqlx::query_scalar(ALBUM_HELD)
        .bind(id.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await?;
    Ok(found.is_some())
}

/// The album rule, which the artist rule embeds.
const ALBUM_HELD: &str = "SELECT 1 AS \"found: i64\" FROM albums a WHERE a.id = ? AND ( \
     EXISTS (SELECT 1 FROM user_library ul \
             WHERE ul.entity_kind = 'album' AND ul.entity_id = a.id) \
     OR EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND (EXISTS (SELECT 1 FROM \
                media_files mf WHERE mf.track_id = t.id) OR EXISTS (SELECT 1 FROM user_library ul \
                WHERE ul.entity_kind = 'track' AND ul.entity_id = t.id))))";

/// The artist rule: saved, or credited on something held.
const ARTIST_HELD: &str = "SELECT 1 AS \"found: i64\" FROM artists a WHERE a.id = ? AND ( \
     EXISTS (SELECT 1 FROM user_library ul \
             WHERE ul.entity_kind = 'artist' AND ul.entity_id = a.id) \
     OR EXISTS (SELECT 1 FROM track_artists ta JOIN tracks t ON t.id = ta.track_id \
                WHERE ta.artist_id = a.id AND (EXISTS (SELECT 1 FROM media_files mf \
                WHERE mf.track_id = t.id) OR EXISTS (SELECT 1 FROM user_library ul \
                WHERE ul.entity_kind = 'track' AND ul.entity_id = t.id))) \
     OR EXISTS (SELECT 1 FROM album_artists aa JOIN albums al ON al.id = aa.album_id \
                WHERE aa.artist_id = a.id AND (EXISTS (SELECT 1 FROM user_library ul \
                WHERE ul.entity_kind = 'album' AND ul.entity_id = al.id) \
                OR EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = al.id AND \
                (EXISTS (SELECT 1 FROM media_files mf WHERE mf.track_id = t.id) \
                OR EXISTS (SELECT 1 FROM user_library ul WHERE ul.entity_kind = 'track' \
                AND ul.entity_id = t.id))))))";
