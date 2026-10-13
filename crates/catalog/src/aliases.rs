//! The `external_ids` alias table: reading which stored row an identifier names.
//!
//! An alias is a namespace and a value pointing at one entity row. The
//! namespaces this crate writes are `isrc`, `upc`, the canonical MusicBrainz
//! namespaces (`mb-recording`, `mb-release`, `mb-release-group`, `mb-artist`),
//! and `p:<slug>` for a provider id issued by one catalog instance.
//!
//! The table is what keeps two identifiers for one recording from becoming two
//! rows: a materialized catalog entity records its provider id, so the original
//! `x_` id keeps resolving, and a track records its ISRC, so a second provider
//! that knows the same recording resolves to the stored row instead of a
//! duplicate. The read side is here; the write side is
//! [`crate::materialize`] and [`crate::identity`].

use std::collections::BTreeMap;

use hearthsound_core::{EntityId, EntityKind, IdPrefix, Slug};
use hearthsound_db::Database;
use uuid::Uuid;

use crate::error::CatalogError;

/// How many aliases one statement asks for.
///
/// The values travel as a JSON array, so one statement can hold a whole page of
/// results; the bound keeps a pathological caller from building a statement no
/// SQLite build wants to parse.
pub(crate) const ALIAS_CHUNK: usize = 256;

/// Returns the namespace of a provider id issued by one instance.
///
/// # Parameters
///
/// - `slug`: the instance slug.
///
/// # Examples
///
/// ```
/// use hearthsound_core::Slug;
/// use hearthsound_catalog::aliases::provider_namespace;
///
/// let slug = Slug::new("deezer").unwrap();
/// assert_eq!(provider_namespace(&slug), "p:deezer");
/// ```
pub fn provider_namespace(slug: &Slug) -> String {
    format!("p:{}", slug.as_str())
}

/// Reads the stored rows a set of provider ids resolve to.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `slug`: the instance whose provider ids are looked up.
/// - `kind`: the entity kind the ids name.
/// - `values`: the provider ids.
///
/// # Returns
///
/// One entry per id that has an alias, keyed by the id.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when the query fails and
/// [`CatalogError::CorruptAlias`] when a stored id does not decode.
pub(crate) async fn provider_aliases(
    database: &Database,
    slug: &Slug,
    kind: EntityKind,
    values: &[String],
) -> Result<BTreeMap<String, EntityId>, CatalogError> {
    let namespace = provider_namespace(slug);
    let mut found = BTreeMap::new();
    for chunk in values.chunks(ALIAS_CHUNK) {
        let rows = lookup(database, &namespace, kind, chunk).await?;
        found.extend(rows);
    }
    Ok(found)
}

/// Reads the stored tracks a set of ISRCs resolve to.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `values`: the ISRCs, already normalized by
///   [`crate::dedupe::normalize_isrc`].
///
/// # Returns
///
/// One entry per ISRC that has an alias, keyed by the ISRC.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when the query fails and
/// [`CatalogError::CorruptAlias`] when a stored id does not decode.
pub(crate) async fn isrc_aliases(
    database: &Database,
    values: &[String],
) -> Result<BTreeMap<String, EntityId>, CatalogError> {
    let mut found = BTreeMap::new();
    for chunk in values.chunks(ALIAS_CHUNK) {
        let rows = lookup(database, "isrc", EntityKind::Track, chunk).await?;
        found.extend(rows);
    }
    Ok(found)
}

/// Looks one namespace and entity kind up for a chunk of values.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `namespace`: the alias namespace.
/// - `kind`: the entity kind the aliases name.
/// - `values`: the alias values, at most [`ALIAS_CHUNK`] of them.
///
/// # Returns
///
/// The values that have an alias, mapped to the row they name.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when the query fails and
/// [`CatalogError::CorruptAlias`] when a stored id does not decode.
async fn lookup(
    database: &Database,
    namespace: &str,
    kind: EntityKind,
    values: &[String],
) -> Result<BTreeMap<String, EntityId>, CatalogError> {
    if values.is_empty() {
        return Ok(BTreeMap::new());
    }
    let encoded = serde_json::to_string(values)?;
    let rows = sqlx::query_as!(
        AliasRow,
        "SELECT value, entity_id FROM external_ids \
         WHERE namespace = ? AND entity_kind = ? \
           AND value IN (SELECT value FROM json_each(?))",
        namespace,
        kind_literal(kind),
        encoded,
    )
    .fetch_all(database.readers())
    .await?;

    let mut found = BTreeMap::new();
    for row in rows {
        found.insert(row.value, entity_id(kind, &row.entity_id)?);
    }
    Ok(found)
}

/// One row of an alias lookup.
struct AliasRow {
    /// The alias value.
    value: String,
    /// The stored row the value names.
    entity_id: Vec<u8>,
}

/// Converts a stored identifier column into a client id of `kind`.
///
/// # Parameters
///
/// - `kind`: the entity kind the id names.
/// - `bytes`: the 16 stored bytes.
///
/// # Returns
///
/// The client id.
///
/// # Errors
///
/// Returns [`CatalogError::CorruptAlias`] when the bytes are not a UUID.
pub(crate) fn entity_id(kind: EntityKind, bytes: &[u8]) -> Result<EntityId, CatalogError> {
    let uuid = Uuid::from_slice(bytes).map_err(|_| CatalogError::CorruptAlias)?;
    Ok(EntityId::from_uuid(prefix(kind), uuid))
}

/// Returns the client id prefix of a catalog entity kind.
///
/// # Parameters
///
/// - `kind`: the entity kind.
fn prefix(kind: EntityKind) -> IdPrefix {
    IdPrefix::from_entity_kind(kind)
}

/// Returns the literal an entity kind has in the `entity_kind` column.
///
/// # Parameters
///
/// - `kind`: the entity kind.
pub(crate) fn kind_literal(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    }
}
