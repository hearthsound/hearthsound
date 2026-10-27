//! The stored identifiers of entities: `external_ids`, read and written.
//!
//! Every provider's identifier for an entity is a row of `external_ids`: a
//! namespace, a value, and the entity row it names. The namespaces the server
//! shares are `isrc`, `upc` and the canonical MusicBrainz ones
//! (`mb-recording`, `mb-release`, `mb-release-group`, `mb-artist`); a catalog
//! instance's own identifier is recorded under `p:<slug>`. Nothing else about an
//! entity's identity is stored in a column, so these functions are the only way
//! the rest of the server reads or writes it.
//!
//! A write that finds an identifier already owned by another row reports a
//! [`Conflict`] instead of failing: two rows that turn out to be one item are
//! merged by the caller ([`crate::entities::merge`]), which is what keeps one
//! identifier from ever becoming two.

use std::collections::HashMap;

use hearthsound_core::EntityKind;
use hearthsound_core::providers::ExternalId;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::decode_uuid;

/// How many entities one statement reads identifiers for.
///
/// The keys travel as a JSON array, so one statement can read a whole page of
/// rows; the bound keeps a pathological caller from building a statement no
/// SQLite build wants to parse.
pub(crate) const ID_CHUNK: usize = 256;

/// One identifier that already names another row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The identifier's namespace.
    pub namespace: String,
    /// The identifier's value.
    pub value: String,
    /// The row that already holds it.
    pub owner: Uuid,
}

/// Returns the `entity_kind` literal of an entity kind.
///
/// # Parameters
///
/// - `kind`: the entity kind.
///
/// # Returns
///
/// The literal stored in `external_ids.entity_kind`.
///
/// # Errors
///
/// Returns [`LibraryError::UnknownLiteral`] for a kind `external_ids` does not
/// store.
pub fn kind_literal(kind: EntityKind) -> Result<&'static str, LibraryError> {
    match kind {
        EntityKind::Track => Ok("track"),
        EntityKind::Album => Ok("album"),
        EntityKind::Artist => Ok("artist"),
        EntityKind::Playlist => Ok("playlist"),
    }
}

/// Attaches every identifier to one entity, reporting the ones another row owns.
///
/// An identifier already attached to the same entity is a no-op. An identifier
/// owned by another row is left where it is and reported, so the caller merges
/// the two rows instead of silently stealing the identity.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
/// - `ids`: the identifiers to attach.
///
/// # Returns
///
/// One [`Conflict`] per identifier that names another row, in the order given.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails and
/// [`LibraryError::UnknownLiteral`] for a kind `external_ids` does not store.
pub async fn attach(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
    ids: &[ExternalId],
) -> Result<Vec<Conflict>, LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let key = entity.as_bytes().to_vec();
    let mut conflicts = Vec::new();
    for id in ids {
        if id.namespace.is_empty() || id.value.is_empty() {
            continue;
        }
        let owner: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT entity_id FROM external_ids \
             WHERE namespace = ? AND entity_kind = ? AND value = ?",
        )
        .bind(&id.namespace)
        .bind(kind_literal)
        .bind(&id.value)
        .fetch_optional(&mut *conn)
        .await?;
        match owner {
            Some(owner) => {
                let owner = decode_uuid(&owner)?;
                if owner != entity {
                    conflicts.push(Conflict {
                        namespace: id.namespace.clone(),
                        value: id.value.clone(),
                        owner,
                    });
                }
            }
            None => {
                sqlx::query(
                    "INSERT INTO external_ids (namespace, value, entity_kind, entity_id) \
                     VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
                )
                .bind(&id.namespace)
                .bind(&id.value)
                .bind(kind_literal)
                .bind(key.as_slice())
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    Ok(conflicts)
}

/// Returns the row an identifier names, when any row holds it.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind the identifier names.
/// - `namespace`: the identifier's namespace.
/// - `value`: the identifier's value.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn owner_of(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    namespace: &str,
    value: &str,
) -> Result<Option<Uuid>, LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let found: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT entity_id FROM external_ids \
         WHERE namespace = ? AND entity_kind = ? AND value = ? LIMIT 1",
    )
    .bind(namespace)
    .bind(kind_literal)
    .bind(value)
    .fetch_optional(&mut *conn)
    .await?;
    match found {
        Some(bytes) => Ok(Some(decode_uuid(&bytes)?)),
        None => Ok(None),
    }
}

/// Reports whether an entity holds any identifier at all.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails and
/// [`LibraryError::UnknownLiteral`] for a kind `external_ids` does not store.
pub async fn is_identified(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
) -> Result<bool, LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 AS \"found: i64\" FROM external_ids \
         WHERE entity_kind = ? AND entity_id = ? LIMIT 1",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(found.is_some())
}

/// Reads every identifier of one entity.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
///
/// # Returns
///
/// The identifiers, ordered by namespace and value.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails and
/// [`LibraryError::UnknownLiteral`] for a kind `external_ids` does not store.
pub async fn ids_of(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
) -> Result<Vec<ExternalId>, LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT namespace, value FROM external_ids \
         WHERE entity_kind = ? AND entity_id = ? ORDER BY namespace, value",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(namespace, value)| ExternalId { namespace, value })
        .collect())
}

/// Reads the identifiers of many entities in one statement.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entities`: the rows' raw primary keys.
///
/// # Returns
///
/// The identifiers of every row that has any, keyed by the row's primary key.
/// A row without identifiers has no entry.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn ids_of_many(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entities: &[Uuid],
) -> Result<HashMap<Uuid, Vec<ExternalId>>, LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let mut grouped: HashMap<Uuid, Vec<ExternalId>> = HashMap::new();
    for chunk in entities.chunks(ID_CHUNK) {
        let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT entity_id, namespace, value FROM external_ids \
             WHERE entity_kind = ",
        );
        builder.push_bind(kind_literal);
        builder.push(" AND entity_id IN (");
        let mut separated = builder.separated(", ");
        for id in chunk {
            separated.push_bind(id.as_bytes().to_vec());
        }
        separated.push_unseparated(")");
        let rows: Vec<(Vec<u8>, String, String)> =
            builder.build_query_as().fetch_all(&mut *conn).await?;
        for (entity, namespace, value) in rows {
            grouped
                .entry(decode_uuid(&entity)?)
                .or_default()
                .push(ExternalId { namespace, value });
        }
    }
    Ok(grouped)
}
