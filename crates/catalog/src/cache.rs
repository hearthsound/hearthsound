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
use hearthsound_core::providers::{ArtistInfoRequest, ArtworkSubject};
use hearthsound_core::providers::{CatalogQuery, RecordingHint};
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

    /// Builds the key of an album reference lookup.
    ///
    /// A reference is the compact album a search result or an artist listing
    /// carries; it is kept under its own key so it never stands in for a
    /// tracklist and a tracklist never stands in for it.
    ///
    /// # Parameters
    ///
    /// - `provider_id`: the provider's own album id.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::cache::RequestKey;
    ///
    /// assert_eq!(
    ///     RequestKey::album_ref("hello-album-1").as_str(),
    ///     "album-ref|id=13:hello-album-1"
    /// );
    /// ```
    pub fn album_ref(provider_id: &str) -> RequestKey {
        RequestKey(format!("album-ref|id={}", encode(provider_id)))
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

    /// Builds the key of a recording-hint resolution request.
    ///
    /// Every part of the hint is part of the key, because two hints that differ
    /// in any of them are two different questions; a hint the caller left out is
    /// written as a dash, which no length-prefixed value can produce.
    ///
    /// # Parameters
    ///
    /// - `hint`: the identity the caller knows about the recording.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::cache::RequestKey;
    /// use hearthsound_core::providers::RecordingHint;
    ///
    /// let hint = RecordingHint {
    ///     title: "Cafe del Mar".to_owned(),
    ///     artists: vec!["Energy 52".to_owned()],
    ///     album: None,
    ///     duration_ms: None,
    ///     isrc: None,
    /// };
    /// assert_eq!(
    ///     RequestKey::recording_hint(&hint).as_str(),
    ///     "recording-hint|title=12:Cafe del Mar|artists=9:Energy 52,|album=-|duration=-|isrc=-"
    /// );
    /// ```
    pub fn recording_hint(hint: &RecordingHint) -> RequestKey {
        let mut artists = String::new();
        for artist in &hint.artists {
            artists.push_str(&encode(artist));
            artists.push(',');
        }
        RequestKey(format!(
            "recording-hint|title={}|artists={artists}|album={}|duration={}|isrc={}",
            encode(&hint.title),
            optional(hint.album.as_deref()),
            hint.duration_ms
                .map(|millis| millis.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            optional(hint.isrc.as_deref()),
        ))
    }

    /// Builds the key of an artwork lookup made for a subject.
    ///
    /// The key names the kind, the title, and every identifier the subject
    /// carries, because two instances asked about the same album must share the
    /// answer while an album and an artist that share a title must not.
    ///
    /// # Parameters
    ///
    /// - `kind`: the kind of entity the picture is for.
    /// - `subject`: the subject the instances are asked about.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::cache::RequestKey;
    /// use hearthsound_core::providers::ArtworkSubject;
    /// use hearthsound_core::EntityKind;
    ///
    /// let subject = ArtworkSubject {
    ///     kind: EntityKind::Album,
    ///     provider_id: None,
    ///     ids: Vec::new(),
    ///     title: "Cafe del Mar".to_owned(),
    ///     artists: vec!["Energy 52".to_owned()],
    /// };
    /// assert_eq!(
    ///     RequestKey::artwork(EntityKind::Album, &subject).as_str(),
    ///     "artwork|kind=album|title=12:Cafe del Mar|artists=9:Energy 52,|ids="
    /// );
    /// ```
    pub fn artwork(kind: EntityKind, subject: &ArtworkSubject) -> RequestKey {
        let mut artists = String::new();
        for artist in &subject.artists {
            artists.push_str(&encode(artist));
            artists.push(',');
        }
        let mut ids = String::new();
        for id in &subject.ids {
            ids.push_str(&encode(&id.namespace));
            ids.push('=');
            ids.push_str(&encode(&id.value));
            ids.push(';');
        }
        RequestKey(format!(
            "artwork|kind={}|title={}|artists={artists}|ids={ids}",
            kind_literal(kind),
            encode(&subject.title),
        ))
    }

    /// Builds the key of an artist-info lookup.
    ///
    /// The key names the artist's name and every identifier the caller has for
    /// them, because two instances asked about the same artist must share the
    /// answer while two artists who share a name must not.
    ///
    /// # Parameters
    ///
    /// - `request`: the artist's name and the identifiers the caller has.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::cache::RequestKey;
    /// use hearthsound_core::providers::{ArtistInfoRequest, ExternalId};
    ///
    /// let request = ArtistInfoRequest {
    ///     name: "Energy 52".to_owned(),
    ///     ids: vec![ExternalId {
    ///         namespace: "mb-artist".to_owned(),
    ///         value: "1111".to_owned(),
    ///     }],
    /// };
    /// assert_eq!(
    ///     RequestKey::artist_info(&request).as_str(),
    ///     "artist-info|name=9:Energy 52|ids=9:mb-artist=4:1111;"
    /// );
    /// ```
    pub fn artist_info(request: &ArtistInfoRequest) -> RequestKey {
        let mut ids = String::new();
        for id in &request.ids {
            ids.push_str(&encode(&id.namespace));
            ids.push('=');
            ids.push_str(&encode(&id.value));
            ids.push(';');
        }
        RequestKey(format!(
            "artist-info|name={}|ids={ids}",
            encode(&request.name)
        ))
    }

    /// Builds the key of a resolution request made by ISRC.
    ///
    /// The namespace differs from [`RequestKey::isrc`], which keys a catalog
    /// layer's ISRC lookup: one instance can serve both layers, and the two
    /// answers are not interchangeable.
    ///
    /// # Parameters
    ///
    /// - `isrc`: the normalized International Standard Recording Code.
    pub fn recording_isrc(isrc: &str) -> RequestKey {
        RequestKey(format!("recording-isrc|value={}", encode(isrc)))
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
    /// A cache tier that cannot be read is a miss, not a failure: a transient
    /// `SQLITE_BUSY` on the cache table must not decide which provider answers a
    /// request, because the next instance in the layer may describe a different
    /// catalog. The read is logged and the call runs instead.
    ///
    /// # Errors
    ///
    /// Returns what `call` returns. A failed call is not cached.
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
        let cached = match self.get::<W>(instance, key).await {
            Ok(cached) => cached,
            Err(error) => {
                tracing::warn!(
                    instance = %instance,
                    request = key.as_str(),
                    error = %error,
                    "a cached catalog response could not be read; asking the provider"
                );
                None
            }
        };
        if let Some(cached) = cached {
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

    /// Reads a response that is still fresh, treating a failure as a miss.
    ///
    /// This is the read every cache-only service method uses: a request that
    /// must not call a provider cannot fail because the cache tier is busy, so a
    /// read failure is logged and reported as no answer.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance id part of the key.
    /// - `key`: the canonical request key.
    ///
    /// # Returns
    ///
    /// The stored response, or `None` when it is absent, stale, undecodable or
    /// unreadable.
    pub(crate) async fn read<T>(&self, instance: Uuid, key: &RequestKey) -> Option<T>
    where
        T: DeserializeOwned,
    {
        match self.get::<T>(instance, key).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    instance = %instance,
                    request = key.as_str(),
                    error = %error,
                    "a cached catalog response could not be read"
                );
                None
            }
        }
    }

    /// Stores responses of one shape in the persistent tier, in one statement.
    ///
    /// A single fill is this call with one entry.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance id part of every key.
    /// - `entries`: the request keys and the responses, in their stored form.
    /// - `ttl`: how long every stored response stays fresh.
    pub(crate) async fn fill_many<W>(
        &self,
        instance: Uuid,
        entries: &[(RequestKey, W)],
        ttl: Duration,
    ) where
        W: Serialize,
    {
        let mut encoded = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            if let Some(json) = encode_for_cache(instance, key, value) {
                encoded.push((key.clone(), json));
            }
        }
        self.fill_encoded(instance, &encoded, ttl).await;
    }

    /// Writes already-encoded responses in one statement.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance id part of every key.
    /// - `entries`: the request keys and their encoded responses.
    /// - `ttl`: how long every stored response stays fresh.
    async fn fill_encoded(
        &self,
        instance: Uuid,
        entries: &[(RequestKey, String)],
        ttl: Duration,
    ) -> bool {
        if entries.is_empty() {
            return false;
        }
        let now = now_millis();
        let expires_at = now.saturating_add(duration_millis(ttl));
        let instance_bytes = instance.as_bytes();
        let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "INSERT INTO catalog_cache (instance_id, request_key, response, fetched_at, \
             expires_at) ",
        );
        builder.push_values(entries, |mut row, (key, json)| {
            row.push_bind(instance_bytes.as_slice())
                .push_bind(key.as_str())
                .push_bind(json.as_str())
                .push_bind(now)
                .push_bind(expires_at);
        });
        builder.push(
            " ON CONFLICT (instance_id, request_key) DO UPDATE SET \
             response = excluded.response, fetched_at = excluded.fetched_at, \
             expires_at = excluded.expires_at \
             WHERE catalog_cache.expires_at IS NOT NULL \
               AND catalog_cache.expires_at <= excluded.fetched_at",
        );
        match builder.build().execute(self.database.writer()).await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    instance = %instance,
                    entries = entries.len(),
                    error = %error,
                    "catalog responses could not be cached"
                );
                false
            }
        }
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

/// Encodes one response for the persistent tier.
///
/// # Parameters
///
/// - `instance`: the instance id, for the log line.
/// - `key`: the request key, for the log line.
/// - `value`: the response, in its stored form.
///
/// # Returns
///
/// The JSON to store, or `None` when it cannot be encoded.
fn encode_for_cache<W>(instance: Uuid, key: &RequestKey, value: &W) -> Option<String>
where
    W: Serialize,
{
    match serde_json::to_string(value) {
        Ok(json) => Some(json),
        Err(error) => {
            tracing::warn!(
                instance = %instance,
                request = key.as_str(),
                error = %error,
                "a catalog response could not be encoded for the cache"
            );
            None
        }
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

/// Encodes an optional argument, marking its absence.
///
/// # Parameters
///
/// - `value`: the argument, or `None` when the caller left it out.
///
/// # Returns
///
/// [`encode`] of the value, or a dash. An encoded value always starts with a
/// digit, so a dash can only mean "no value".
fn optional(value: Option<&str>) -> String {
    match value {
        Some(value) => encode(value),
        None => "-".to_owned(),
    }
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
