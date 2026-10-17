//! What one user has done with a page of entities.
//!
//! A listing endpoint answers with the entities a client asked for, and the
//! per-user parts of each entity - whether the user liked it, how they rated it,
//! how often they played it, and when they last did - come from three tables.
//! [`load`] reads them for a whole page at once, so a page of a hundred albums
//! costs a bounded number of statements instead of one per entity.
//!
//! The three reads are one per table, each over the ids of the page, and the
//! results are merged into one [`Annotation`] per entity. The ids of one read
//! are chunked, because a statement that binds an unbounded list is both slow
//! and limited by SQLite's parameter count.

use std::collections::{HashMap, HashSet};

use hearthsound_core::EntityKind;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};
use uuid::Uuid;

use crate::error::ActivityError;

/// How many ids one statement binds.
pub const MAX_IDS_PER_QUERY: usize = 500;

/// One user's relationship to one entity.
///
/// Every field is optional or zero when the user has done nothing with the
/// entity, so a caller can emit it without a second lookup.
///
/// # Examples
///
/// ```
/// use hearthsound_activity::Annotation;
///
/// let plain = Annotation::default();
/// assert_eq!(plain.play_count, 0);
/// assert!(plain.starred_at.is_none());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Annotation {
    /// When the user liked the entity, in unix milliseconds.
    pub starred_at: Option<i64>,
    /// The user's rating, 1 to 5.
    pub rating: Option<u8>,
    /// How many times the user played the entity.
    pub play_count: i64,
    /// When the user last played the entity, in unix milliseconds.
    pub last_played: Option<i64>,
}

/// One stored id with the instant a like happened.
#[derive(sqlx::FromRow)]
struct LikeRow {
    /// The stored entity id.
    entity_id: Vec<u8>,
    /// When the like happened.
    at: i64,
}

/// One stored id with its rating.
#[derive(sqlx::FromRow)]
struct RatingRow {
    /// The stored entity id.
    entity_id: Vec<u8>,
    /// The stored rating.
    rating: i64,
}

/// One stored id with its play count and last play.
#[derive(sqlx::FromRow)]
struct PlayRow {
    /// The stored entity id, which is a track id for a track listing and an
    /// album id for an album listing.
    entity_id: Vec<u8>,
    /// How many plays the user has.
    plays: i64,
    /// When the user last played it.
    last_played: Option<i64>,
}

/// Loads what a user has done with a page of entities.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `user_id`: the account whose activity is read.
/// - `kind`: the kind of entity the ids name. An artist listing carries no play
///   fields, so its plays are not read and stay at zero.
/// - `ids`: the entities of the page, in any order.
///
/// # Returns
///
/// One annotation per id that has something to report; an id the user has done
/// nothing with is absent, which a caller reads as [`Annotation::default`].
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when a statement fails and
/// [`ActivityError::CorruptId`] when a stored id is not 16 bytes.
pub async fn load(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Annotation>, ActivityError> {
    let mut annotations: HashMap<Uuid, Annotation> = HashMap::new();
    for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
        load_likes(conn, user_id, kind, chunk, &mut annotations).await?;
        load_ratings(conn, user_id, kind, chunk, &mut annotations).await?;
        // ArtistID3 carries no play fields, so an artist's plays are not read.
        if kind != EntityKind::Artist {
            load_plays(conn, user_id, kind, chunk, &mut annotations).await?;
        }
    }
    Ok(annotations)
}

/// Reports which of the given entities the user has disliked.
///
/// A dislike is an active `user_feedback` row with `value = 'dislike'`. The ids
/// are read in chunks of [`MAX_IDS_PER_QUERY`].
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `user_id`: the account whose dislikes are read.
/// - `kind`: the kind of entity the ids name.
/// - `ids`: the entities to test, in any order.
///
/// # Returns
///
/// The ids of `ids` that the user has disliked; every other id is absent.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when a statement fails and
/// [`ActivityError::CorruptId`] when a stored id is not 16 bytes.
pub async fn disliked_ids(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    ids: &[Uuid],
) -> Result<HashSet<Uuid>, ActivityError> {
    let mut disliked = HashSet::new();
    for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT entity_id FROM user_feedback \
             WHERE value = 'dislike' AND user_id = ",
        );
        builder.push_bind(user_id.as_bytes().to_vec());
        builder.push(" AND entity_kind = ");
        builder.push_bind(kind_literal(kind));
        push_id_list(&mut builder, chunk);
        let rows: Vec<Vec<u8>> = builder.build_query_scalar().fetch_all(&mut *conn).await?;
        for bytes in rows {
            disliked.insert(decode(&bytes)?);
        }
    }
    Ok(disliked)
}

/// Loads the likes of one chunk of ids.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `user_id`: the account whose likes are read.
/// - `kind`: the kind of entity the ids name.
/// - `ids`: the chunk of ids.
/// - `annotations`: the map the results are merged into.
///
/// # Errors
///
/// Returns [`ActivityError`] when the statement fails or an id does not decode.
async fn load_likes(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    ids: &[Uuid],
    annotations: &mut HashMap<Uuid, Annotation>,
) -> Result<(), ActivityError> {
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT entity_id, at FROM user_feedback \
         WHERE value = 'like' AND user_id = ",
    );
    builder.push_bind(user_id.as_bytes().to_vec());
    builder.push(" AND entity_kind = ");
    builder.push_bind(kind_literal(kind));
    push_id_list(&mut builder, ids);
    let rows = builder
        .build_query_as::<LikeRow>()
        .fetch_all(&mut *conn)
        .await?;
    for row in rows {
        let id = decode(&row.entity_id)?;
        annotations.entry(id).or_default().starred_at = Some(row.at);
    }
    Ok(())
}

/// Loads the ratings of one chunk of ids.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `user_id`: the account whose ratings are read.
/// - `kind`: the kind of entity the ids name.
/// - `ids`: the chunk of ids.
/// - `annotations`: the map the results are merged into.
///
/// # Errors
///
/// Returns [`ActivityError`] when the statement fails or a stored rating is not
/// a byte or an id does not decode.
async fn load_ratings(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    ids: &[Uuid],
    annotations: &mut HashMap<Uuid, Annotation>,
) -> Result<(), ActivityError> {
    let mut builder =
        QueryBuilder::<Sqlite>::new("SELECT entity_id, rating FROM user_ratings WHERE user_id = ");
    builder.push_bind(user_id.as_bytes().to_vec());
    builder.push(" AND entity_kind = ");
    builder.push_bind(kind_literal(kind));
    push_id_list(&mut builder, ids);
    let rows = builder
        .build_query_as::<RatingRow>()
        .fetch_all(&mut *conn)
        .await?;
    for row in rows {
        let id = decode(&row.entity_id)?;
        let rating = u8::try_from(row.rating).unwrap_or_default();
        annotations.entry(id).or_default().rating = Some(rating);
    }
    Ok(())
}

/// Loads the play counts of one chunk of ids.
///
/// An album's plays are the sum over its tracks, so the ids of an album listing
/// are joined against the tracks the plays name.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `user_id`: the account whose plays are read.
/// - `kind`: the kind of entity the ids name.
/// - `ids`: the chunk of ids.
/// - `annotations`: the map the results are merged into.
///
/// # Errors
///
/// Returns [`ActivityError`] when the statement fails or an id does not decode.
async fn load_plays(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    ids: &[Uuid],
    annotations: &mut HashMap<Uuid, Annotation>,
) -> Result<(), ActivityError> {
    let (select, column) = match kind {
        EntityKind::Album => (
            "SELECT t.album_id AS entity_id, count(*) AS plays, max(p.started_at) AS last_played \
             FROM plays p JOIN tracks t ON t.id = p.track_id WHERE p.user_id = ",
            "t.album_id",
        ),
        _ => (
            "SELECT track_id AS entity_id, count(*) AS plays, max(started_at) AS last_played \
             FROM plays WHERE user_id = ",
            "track_id",
        ),
    };
    let mut builder = QueryBuilder::<Sqlite>::new(select);
    builder.push_bind(user_id.as_bytes().to_vec());
    builder.push(" AND ").push(column).push(" IN (");
    let mut separated = builder.separated(", ");
    for id in ids {
        separated.push_bind(id.as_bytes().to_vec());
    }
    separated.push_unseparated(")");
    separated.push_unseparated(" GROUP BY ");
    separated.push_unseparated(column);
    let rows = builder
        .build_query_as::<PlayRow>()
        .fetch_all(&mut *conn)
        .await?;
    for row in rows {
        let id = decode(&row.entity_id)?;
        let entry = annotations.entry(id).or_default();
        entry.play_count = row.plays;
        entry.last_played = row.last_played;
    }
    Ok(())
}

/// Appends an `AND entity_id IN (...)` clause over the ids.
///
/// # Parameters
///
/// - `builder`: the builder to append to.
/// - `ids`: the ids to bind.
fn push_id_list(builder: &mut QueryBuilder<Sqlite>, ids: &[Uuid]) {
    builder.push(" AND entity_id IN (");
    let mut separated = builder.separated(", ");
    for id in ids {
        separated.push_bind(id.as_bytes().to_vec());
    }
    separated.push_unseparated(")");
}

/// Decodes a stored entity id.
///
/// # Parameters
///
/// - `bytes`: the stored bytes.
///
/// # Returns
///
/// The id.
///
/// # Errors
///
/// Returns [`ActivityError::CorruptId`] when the bytes are not 16 bytes.
fn decode(bytes: &[u8]) -> Result<Uuid, ActivityError> {
    Uuid::from_slice(bytes).map_err(ActivityError::from)
}

/// Returns the `user_feedback` and `user_ratings` literal of a kind.
///
/// # Parameters
///
/// - `kind`: the kind to name.
///
/// # Returns
///
/// The literal the columns store.
fn kind_literal(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    }
}
