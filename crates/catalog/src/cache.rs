//! The two-level response cache.
//!
//! A provider call costs quota, so every successful answer is kept twice: in
//! this process, where a repeated search in one session is answered without
//! touching the database, and in the `catalog_cache` table, where it survives a
//! restart and is shared with anything else that reads the same database.
//!
//! Both tiers are keyed by `(instance id, request key)`. The instance is part of
//! the key on purpose: two instances of the same provider have separate
//! credentials and separate quotas, so they must not share answers.
//!
//! The request key is a canonical string built by [`RequestKey`]: the operation,
//! then every argument in a fixed order, with free-form arguments length
//! prefixed so that no two different requests can produce the same key.
//!
//! Only successful responses are cached. A failure is not: a provider that was
//! briefly unavailable or that rate limited the caller has to be asked again,
//! and a not-found has to be able to turn into a found as soon as the provider
//! knows the entity.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use hearthsound_core::EntityKind;
use hearthsound_core::providers::CatalogQuery;
use hearthsound_db::Database;
use moka::future::Cache as MemoryCache;
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::error::CatalogError;

/// How long a cached search answer stays fresh.
///
/// Five minutes. A search is a cheap call on the provider's side but a repeated
/// one in a session, a client typing into a search box asks the same query over
/// and over, and a stale-by-five-minutes answer is indistinguishable to a
/// listener. Search results churn with the provider's own catalog, so this is
/// the short of the two lifetimes.
pub const SEARCH_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// How long a cached entity lookup stays fresh.
///
/// Six hours. A track, album or artist changes far less often than a search
/// result set, and a lookup is the call a save, a stream or a cover art request
/// makes, so the answer is worth keeping for much longer. Twelve times the
/// search lifetime is still a fraction of the provider's own update cadence for
/// a recording's metadata.
pub const ENTITY_CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// How many responses the in-memory tier keeps.
///
/// A bound, not a target: the in-memory tier exists to answer the handful of
/// requests a browsing session repeats, and a catalog response can be a whole
/// album's tracklist, so the cache holds a fixed number of entries and lets the
/// least recently used ones fall back to the table.
const MEMORY_CAPACITY: u64 = 1_024;

/// The canonical key of one cacheable catalog request.
///
/// The key is a string a row's `request_key` column holds. Its form is an
/// implementation detail; what matters is that two equal requests produce equal
/// keys and two different requests never do, which is why a free-form argument
/// (a search text, a provider id) is written as its byte length, a colon, and
/// the value.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::cache::RequestKey;
///
/// let key = RequestKey::track("hello-track-1");
/// assert_eq!(key.as_str(), "track|id=13:hello-track-1");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestKey(String);

impl RequestKey {
    /// Builds the key of a search request.
    ///
    /// # Parameters
    ///
    /// - `query`: the query as it is sent to the provider, with the result cap
    ///   already applied.
    pub fn search(query: &CatalogQuery) -> RequestKey {
        let mut kinds = String::new();
        for (index, kind) in query.kinds.iter().enumerate() {
            if index > 0 {
                kinds.push(',');
            }
            kinds.push_str(kind_literal(*kind));
        }
        RequestKey(format!(
            "search|kinds={kinds}|limit={}|offset={}|text={}",
            query.limit,
            query.offset,
            encode(&query.text)
        ))
    }

    /// Builds the key of a track lookup.
    ///
    /// # Parameters
    ///
    /// - `provider_id`: the provider's own track id.
    pub fn track(provider_id: &str) -> RequestKey {
        RequestKey(format!("track|id={}", encode(provider_id)))
    }

    /// Builds the key of an album lookup.
    ///
    /// # Parameters
    ///
    /// - `provider_id`: the provider's own album id.
    pub fn album(provider_id: &str) -> RequestKey {
        RequestKey(format!("album|id={}", encode(provider_id)))
    }

    /// Builds the key of an artist lookup.
    ///
    /// # Parameters
    ///
    /// - `provider_id`: the provider's own artist id.
    pub fn artist(provider_id: &str) -> RequestKey {
        RequestKey(format!("artist|id={}", encode(provider_id)))
    }

    /// Builds the key of an artist album listing.
    ///
    /// # Parameters
    ///
    /// - `provider_id`: the provider's own artist id.
    /// - `limit`: the maximum number of albums asked for.
    /// - `offset`: the number of albums skipped.
    pub fn artist_albums(provider_id: &str, limit: u32, offset: u32) -> RequestKey {
        RequestKey(format!(
            "artist-albums|id={}|limit={limit}|offset={offset}",
            encode(provider_id)
        ))
    }

    /// Builds the key of an artist top-track listing.
    ///
    /// # Parameters
    ///
    /// - `provider_id`: the provider's own artist id.
    /// - `limit`: the maximum number of tracks asked for.
    pub fn artist_top_tracks(provider_id: &str, limit: u32) -> RequestKey {
        RequestKey(format!(
            "artist-top-tracks|id={}|limit={limit}",
            encode(provider_id)
        ))
    }

    /// Builds the key of an ISRC lookup.
    ///
    /// # Parameters
    ///
    /// - `isrc`: the International Standard Recording Code.
    pub fn isrc(isrc: &str) -> RequestKey {
        RequestKey(format!("isrc|value={}", encode(isrc)))
    }

    /// Returns the key as the string a cache row stores.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestKey {
    /// Writes the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The two-level cache of provider responses.
pub(crate) struct Cache {
    /// The in-memory tier, which dies with the process.
    memory: MemoryCache<(Uuid, RequestKey), Arc<Entry>>,
    /// The database whose `catalog_cache` table is the persistent tier.
    database: Arc<Database>,
}

/// One in-memory cache entry.
struct Entry {
    /// The stored response, as the JSON the table would hold.
    json: Arc<str>,
    /// The instant the entry stops being fresh, in unix milliseconds.
    expires_at_ms: i64,
}

impl Cache {
    /// Builds an empty cache over a database.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database, whose `catalog_cache` table the
    ///   persistent tier reads and writes.
    pub(crate) fn new(database: Arc<Database>) -> Cache {
        let memory = MemoryCache::builder()
            .max_capacity(MEMORY_CAPACITY)
            .time_to_live(ENTITY_CACHE_TTL)
            .build();
        Cache { memory, database }
    }

    /// Runs a provider call through the cache.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance id part of the key.
    /// - `key`: the canonical request key.
    /// - `ttl`: how long a fresh answer stays usable.
    /// - `call`: the provider call to run on a miss. It is awaited only when
    ///   both tiers miss, so a cache hit never touches the provider.
    ///
    /// # Returns
    ///
    /// The cached response, or the response the call produced.
    ///
    /// # Errors
    ///
    /// Returns what `call` returns, and a read failure of the persistent tier.
    /// A call that failed is not cached.
    pub(crate) async fn through<T, W, F>(
        &self,
        instance: Uuid,
        key: &RequestKey,
        ttl: Duration,
        call: F,
    ) -> Result<T, CatalogError>
    where
        W: Serialize + DeserializeOwned + for<'a> From<&'a T> + Into<T>,
        F: Future<Output = Result<T, CatalogError>>,
    {
        if let Some(cached) = self.get::<W>(instance, key).await? {
            return Ok(cached.into());
        }
        let value = call.await?;
        self.put(instance, key, &W::from(&value), ttl).await;
        Ok(value)
    }

    /// Reads a response that is still fresh.
    ///
    /// The in-memory tier is asked first; a miss reads the table, which fills
    /// the in-memory tier again with the lifetime the row has left.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance id part of the key.
    /// - `key`: the canonical request key.
    ///
    /// # Returns
    ///
    /// The stored response, or `None` when neither tier holds a fresh one.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the table cannot be read. A row
    /// that is stale, or whose stored JSON no longer decodes, is a miss with a
    /// log line and not an error.
    pub(crate) async fn get<T>(
        &self,
        instance: Uuid,
        key: &RequestKey,
    ) -> Result<Option<T>, CatalogError>
    where
        T: DeserializeOwned,
    {
        let now = now_millis();
        if let Some(entry) = self.memory.get(&(instance, key.clone())).await
            && entry.expires_at_ms > now
        {
            match decode::<T>(key, &entry.json) {
                Some(value) => return Ok(Some(value)),
                None => self.memory.invalidate(&(instance, key.clone())).await,
            }
        }

        let row = sqlx::query!(
            "SELECT response, expires_at FROM catalog_cache \
             WHERE instance_id = ? AND request_key = ?",
            instance.as_bytes().as_slice(),
            key.as_str(),
        )
        .fetch_optional(self.database.readers())
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        if let Some(expires_at) = row.expires_at
            && expires_at <= now
        {
            tracing::debug!(
                instance = %instance,
                request = key.as_str(),
                "a cached catalog response is stale; asking the provider again"
            );
            return Ok(None);
        }
        let Some(value) = decode::<T>(key, &row.response) else {
            return Ok(None);
        };

        let expires_at_ms = row
            .expires_at
            .unwrap_or_else(|| now.saturating_add(duration_millis(ENTITY_CACHE_TTL)));
        self.memory
            .insert(
                (instance, key.clone()),
                Arc::new(Entry {
                    json: Arc::from(row.response.as_str()),
                    expires_at_ms,
                }),
            )
            .await;
        Ok(Some(value))
    }

    /// Stores a response in both tiers.
    ///
    /// A store that fails is logged and ignored: the caller already holds a
    /// correct response, and a search must not fail because its answer could not
    /// be cached.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance id part of the key.
    /// - `key`: the canonical request key.
    /// - `value`: the response, in its stored form.
    /// - `ttl`: how long the stored response stays fresh.
    pub(crate) async fn put<W>(&self, instance: Uuid, key: &RequestKey, value: &W, ttl: Duration)
    where
        W: Serialize,
    {
        let json = match serde_json::to_string(value) {
            Ok(json) => json,
            Err(error) => {
                tracing::warn!(
                    instance = %instance,
                    request = key.as_str(),
                    error = %error,
                    "a catalog response could not be encoded for the cache"
                );
                return;
            }
        };
        let now = now_millis();
        let expires_at_ms = now.saturating_add(duration_millis(ttl));

        let written = sqlx::query!(
            "INSERT INTO catalog_cache (instance_id, request_key, response, fetched_at, \
                                        expires_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (instance_id, request_key) DO UPDATE SET \
                response = excluded.response, \
                fetched_at = excluded.fetched_at, \
                expires_at = excluded.expires_at",
            instance.as_bytes().as_slice(),
            key.as_str(),
            json,
            now,
            expires_at_ms,
        )
        .execute(self.database.writer())
        .await;
        if let Err(error) = written {
            tracing::warn!(
                instance = %instance,
                request = key.as_str(),
                error = %error,
                "a catalog response could not be cached"
            );
            return;
        }

        self.memory
            .insert(
                (instance, key.clone()),
                Arc::new(Entry {
                    json: Arc::from(json.as_str()),
                    expires_at_ms,
                }),
            )
            .await;
    }

    /// Deletes every expired row.
    ///
    /// A non-expiring row is never deleted: a `NULL` `expires_at` means the
    /// entry was stored deliberately without a lifetime.
    ///
    /// # Returns
    ///
    /// The number of rows deleted.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the delete fails.
    pub(crate) async fn sweep(&self) -> Result<u64, CatalogError> {
        let deleted = sqlx::query!(
            "DELETE FROM catalog_cache WHERE expires_at IS NOT NULL AND expires_at <= ?",
            now_millis(),
        )
        .execute(self.database.writer())
        .await?;
        Ok(deleted.rows_affected())
    }
}

/// Decodes a stored response, reporting a shape the mirror no longer knows.
///
/// # Parameters
///
/// - `key`: the request key, for the log line.
/// - `json`: the stored JSON.
///
/// # Returns
///
/// The decoded value, or `None` when it does not decode. A row that no longer
/// decodes is a cache miss: the stored shape is an internal format, and an
/// upgrade may change it.
fn decode<T: DeserializeOwned>(key: &RequestKey, json: &str) -> Option<T> {
    match serde_json::from_str(json) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::debug!(
                request = key.as_str(),
                error = %error,
                "a cached catalog response no longer decodes; asking the provider again"
            );
            None
        }
    }
}

/// Encodes a free-form request argument so it cannot be confused with the rest
/// of the key.
///
/// # Parameters
///
/// - `value`: the argument.
///
/// # Returns
///
/// The argument's byte length, a colon, and the argument.
fn encode(value: &str) -> String {
    format!("{}:{value}", value.len())
}

/// Returns the literal an entity kind is written as in a request key.
///
/// The literals are the `entity-kind` literals of the plugin interface. They are
/// part of a stored key, so they must not be renamed.
///
/// # Parameters
///
/// - `kind`: the kind to write.
fn kind_literal(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    }
}

/// Converts a duration to whole milliseconds, saturating at the 64-bit bound.
///
/// # Parameters
///
/// - `duration`: the duration to convert.
fn duration_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Returns the current time in unix milliseconds.
pub(crate) fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a search query for the key tests.
    fn query(text: &str, kinds: Vec<EntityKind>) -> CatalogQuery {
        CatalogQuery {
            text: text.to_owned(),
            kinds,
            limit: 20,
            offset: 0,
        }
    }

    #[test]
    fn a_search_key_depends_on_every_argument() {
        let base = RequestKey::search(&query("energy 52", vec![EntityKind::Track]));

        let other_text = RequestKey::search(&query("energy 53", vec![EntityKind::Track]));
        let mut other_kinds = query("energy 52", vec![EntityKind::Album]);
        other_kinds.limit = 21;

        assert_ne!(base, other_text);
        assert_ne!(
            base,
            RequestKey::search(&other_kinds),
            "the kind and limit are part of the key"
        );
    }

    #[test]
    fn free_form_arguments_cannot_masquerade_as_other_arguments() {
        let first = RequestKey::track("a|id=1:b");
        let second = RequestKey::track("a");

        assert_ne!(first, second);
        assert_eq!(first.as_str(), "track|id=8:a|id=1:b");
    }
}
