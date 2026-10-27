//! What each provider instance said about each entity, stored.
//!
//! `entity_sources` holds one row per entity, instance and operation: the
//! provider's answer, the status (`found` or `missing`) and when it arrived. The
//! fields of an entity are recomputed from these rows ([`crate::fields`]), so a
//! second plugin, a reordering of the layers or an administrator's precedence
//! change never needs another provider call.
//!
//! Payloads are the JSON mirrors of the core provider records
//! ([`crate::wire`]). `PAYLOAD_VERSION` is bumped whenever a stored shape
//! changes, and a row of another version is treated as absent: it is fetched
//! again rather than misread.

use hearthsound_core::EntityKind;
use hearthsound_library::external_ids::kind_literal;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::CatalogError;

/// The version of the payload shapes this build writes and reads.
pub const PAYLOAD_VERSION: i64 = 1;

/// The provider operation one stored answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// The catalog's own view of an entity.
    Catalog,
    /// A catalog's discography listing of an artist.
    CatalogAlbums,
    /// A catalog's top tracks of an artist.
    CatalogTopTracks,
    /// A metadata provider's release materialization.
    Release,
    /// A metadata provider's artist information.
    ArtistInfo,
    /// An artwork provider's images of an entity.
    Artwork,
    /// A discovery provider's similar artists.
    SimilarArtists,
}

impl Operation {
    /// Returns the literal stored in `entity_sources.operation`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Operation::Catalog => "catalog",
            Operation::CatalogAlbums => "catalog-albums",
            Operation::CatalogTopTracks => "catalog-top-tracks",
            Operation::Release => "release",
            Operation::ArtistInfo => "artist-info",
            Operation::Artwork => "artwork",
            Operation::SimilarArtists => "similar-artists",
        }
    }

    /// Parses a stored operation literal.
    ///
    /// # Parameters
    ///
    /// - `value`: the stored literal.
    ///
    /// # Returns
    ///
    /// The operation, or `None` for an unknown literal.
    pub fn parse(value: &str) -> Option<Operation> {
        match value {
            "catalog" => Some(Operation::Catalog),
            "catalog-albums" => Some(Operation::CatalogAlbums),
            "catalog-top-tracks" => Some(Operation::CatalogTopTracks),
            "release" => Some(Operation::Release),
            "artist-info" => Some(Operation::ArtistInfo),
            "artwork" => Some(Operation::Artwork),
            "similar-artists" => Some(Operation::SimilarArtists),
            _ => None,
        }
    }
}

/// Whether a provider had the entity at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The provider answered with the entity.
    Found,
    /// The provider does not know the entity.
    Missing,
}

impl Status {
    /// Returns the literal stored in `entity_sources.status`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Found => "found",
            Status::Missing => "missing",
        }
    }
}

/// One `entity_sources` row as it is read back.
type SourceRow = (Vec<u8>, String, String, Option<String>, i64);

/// One stored provider answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The instance that answered.
    pub instance: Uuid,
    /// The operation the answer belongs to.
    pub operation: Operation,
    /// Whether the provider had the entity.
    pub status: Status,
    /// The payload, for a found answer.
    pub payload: Option<String>,
    /// When the answer arrived, in unix milliseconds.
    pub fetched_at: i64,
}

impl Source {
    /// Returns whether the answer is a found one this build can read.
    pub fn has_payload(&self) -> bool {
        self.status == Status::Found && self.payload.is_some()
    }
}

/// Stores one answer, replacing an earlier one from the same instance.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
/// - `instance`: the instance that answered.
/// - `operation`: the operation the answer belongs to.
/// - `status`: whether the provider had the entity.
/// - `payload`: the JSON payload, for a found answer.
///
/// # Errors
///
/// Returns [`CatalogError::Library`] when the write fails.
pub async fn put(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
    instance: Uuid,
    operation: Operation,
    status: Status,
    payload: Option<&str>,
) -> Result<(), CatalogError> {
    let kind_literal = kind_literal(kind).map_err(CatalogError::Library)?;
    sqlx::query(
        "INSERT INTO entity_sources (entity_kind, entity_id, instance_id, operation, status, \
                                     payload, payload_version, fetched_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (entity_kind, entity_id, instance_id, operation) DO UPDATE SET \
             status = excluded.status, payload = excluded.payload, \
             payload_version = excluded.payload_version, fetched_at = excluded.fetched_at",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .bind(instance.as_bytes().as_slice())
    .bind(operation.as_str())
    .bind(status.as_str())
    .bind(payload)
    .bind(PAYLOAD_VERSION)
    .bind(unix_millis())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Reads every answer stored for one entity.
///
/// Rows of another payload version are skipped, so a shape change re-fetches
/// instead of misreading.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
///
/// # Returns
///
/// The answers, ordered by operation and instance.
///
/// # Errors
///
/// Returns [`CatalogError::Library`] when the query fails and
/// [`CatalogError::CorruptId`] when a stored id cannot be decoded.
pub async fn all(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
) -> Result<Vec<Source>, CatalogError> {
    let kind_literal = kind_literal(kind).map_err(CatalogError::Library)?;
    let rows: Vec<SourceRow> = sqlx::query_as(
        "SELECT instance_id, operation, status, payload, fetched_at FROM entity_sources \
         WHERE entity_kind = ? AND entity_id = ? AND payload_version = ? \
         ORDER BY operation, instance_id",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .bind(PAYLOAD_VERSION)
    .fetch_all(&mut *conn)
    .await?;
    let mut sources = Vec::with_capacity(rows.len());
    for (instance, operation, status, payload, fetched_at) in rows {
        let Some(operation) = Operation::parse(&operation) else {
            continue;
        };
        sources.push(Source {
            instance: Uuid::from_slice(&instance).map_err(|_| CatalogError::CorruptId)?,
            operation,
            status: if status == "missing" {
                Status::Missing
            } else {
                Status::Found
            },
            payload,
            fetched_at,
        });
    }
    Ok(sources)
}

/// Reads the stored answers of one operation for one entity.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
/// - `operation`: the operation to keep.
///
/// # Returns
///
/// The found answers of that operation, in instance order.
///
/// # Errors
///
/// Returns the errors of [`all`].
pub async fn of_operation(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
    operation: Operation,
) -> Result<Vec<Source>, CatalogError> {
    Ok(all(conn, kind, entity)
        .await?
        .into_iter()
        .filter(|source| source.operation == operation && source.has_payload())
        .collect())
}

/// Returns the current time in unix milliseconds.
fn unix_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
