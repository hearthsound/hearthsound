//! Search across the library and the catalog layer.
//!
//! A search answers with stored rows: the listener's own library first (the rows
//! its scopes hold), then the first catalog instance in the administrator's
//! order, whose hits are resolved into rows as they arrive. A hit the library
//! already holds is not repeated, and neither is a catalog artist or album whose
//! folded name equals a local hit's: the local row is the same item, found
//! twice.
//!
//! Provider hits are cached as ids (`answer_cache`), never as copies of provider
//! records, so a cached answer survives a merge and never names a row that no
//! longer exists.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use hearthsound_core::providers::{CatalogQuery, CatalogResults};
use hearthsound_core::{EntityId, EntityKind, IdPrefix, ProviderError};
use hearthsound_db::Database;
use hearthsound_library::query::{self, Scope};
use uuid::Uuid;

use hearthsound_config::keys;

use crate::error::CatalogError;
use crate::normalize::normalize;
use crate::resolve::EntityResolver;
use crate::views::{AlbumRef, ArtistRef, TrackRef};
use crate::wire::WireResults;

/// How long a stored search answer is reused.
pub const ANSWER_TTL: Duration = Duration::from_secs(300);

/// One search's results, as the APIs render them.
#[derive(Debug, Clone, Default)]
pub struct SearchAnswer {
    /// Matching artists, best first.
    pub artists: Vec<ArtistRef>,
    /// Matching albums, best first.
    pub albums: Vec<AlbumRef>,
    /// Matching tracks, best first.
    pub tracks: Vec<TrackRef>,
}

/// The services a search needs.
pub struct SearchService {
    database: std::sync::Arc<Database>,
    registry: std::sync::Arc<hearthsound_plugin_host::ProviderRegistry>,
    settings: hearthsound_config::settings::Settings,
    resolver: EntityResolver,
}

impl SearchService {
    /// Builds the search service.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `registry`: the provider registry.
    /// - `settings`: the settings store, read for the search policy.
    /// - `resolver`: the resolver provider hits enter through.
    pub fn new(
        database: std::sync::Arc<Database>,
        registry: std::sync::Arc<hearthsound_plugin_host::ProviderRegistry>,
        settings: hearthsound_config::settings::Settings,
        resolver: EntityResolver,
    ) -> SearchService {
        SearchService {
            database,
            registry,
            settings,
            resolver,
        }
    }

    /// Searches the library and, when it is enabled, the catalog layer.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener whose library is searched.
    /// - `text`: the query text.
    /// - `kinds`: the entity kinds to return.
    /// - `limit`: the most hits per kind.
    /// - `offset`: how many hits per kind to skip.
    ///
    /// # Returns
    ///
    /// The answer; empty when the text holds nothing searchable.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read or write fails.
    pub async fn search(
        &self,
        user: Uuid,
        text: &str,
        kinds: &[EntityKind],
        limit: u32,
        offset: u32,
    ) -> Result<SearchAnswer, CatalogError> {
        let settings = self.settings.current();
        let enabled = settings.get(keys::SEARCH_CATALOG_ENABLED).unwrap_or(true);
        let catalog_limit = settings
            .get(keys::SEARCH_CATALOG_MAX_RESULTS)
            .unwrap_or(20)
            .clamp(1, 60) as u32;

        let mut connection = self.database.writer().acquire().await?;
        let local = query::search(
            &mut connection,
            Scope::Personal { user_id: user },
            text,
            kinds,
            i64::from(limit),
            i64::from(offset),
        )
        .await
        .map_err(CatalogError::Library)?;
        drop(connection);

        let mut answer = SearchAnswer::default();
        let mut seen: HashSet<String> = HashSet::new();
        for artist in local.artists {
            seen.insert(format!("artist:{}", normalize(&artist.name)));
            answer.artists.push(crate::views::ArtistRef {
                has_image: false,
                artist,
            });
        }
        for album in local.albums {
            seen.insert(format!("album:{}", normalize(&album.title)));
            let has_image = false;
            answer.albums.push(AlbumRef {
                album,
                artist: None,
                has_image,
            });
        }
        for view in local.tracks {
            let track = view.track;
            seen.insert(format!("track:{}", track.id.uuid()));
            answer.tracks.push(TrackRef {
                has_image: false,
                album_title: None,
                artists: Vec::new(),
                track,
            });
        }

        if !enabled {
            return Ok(answer);
        }

        let query = CatalogQuery {
            text: text.to_owned(),
            kinds: kinds.to_vec(),
            limit: catalog_limit,
            offset,
        };
        // Each instance gets its own slice of the layer's budget. One shared
        // deadline let the first instance spend all of it - an instance at its
        // rate limit waits inside its own call, so a slow or refusing primary
        // left the fallback with a deadline already in the past and the search
        // answered nothing while a healthy instance behind it was never asked.
        let budget = std::time::Duration::from_millis(
            settings
                .get(keys::SEARCH_CATALOG_TIMEOUT_MS)
                .unwrap_or(4000)
                .clamp(250, 30_000) as u64,
        );
        let instances: Vec<_> = self
            .registry
            .layers(hearthsound_core::Layer::Catalog)
            .into_iter()
            .filter(|instance| instance.catalog().is_some())
            .collect();
        let slice = if instances.is_empty() {
            budget
        } else {
            budget / u32::try_from(instances.len()).unwrap_or(1)
        };
        let mut refusals: Vec<(String, String)> = Vec::new();
        for instance in instances {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let deadline = Instant::now() + slice;
            let Some(cached) = self
                .cached(instance.id(), text, kinds, limit, offset)
                .await?
            else {
                let results = match tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    provider.search(&query),
                )
                .await
                {
                    Ok(Ok(results)) => results,
                    Ok(Err(ProviderError::NotFound)) => CatalogResults {
                        tracks: Vec::new(),
                        albums: Vec::new(),
                        artists: Vec::new(),
                    },
                    Ok(Err(error)) => {
                        refusals.push((instance.slug().as_str().to_owned(), error.to_string()));
                        continue;
                    }
                    Err(_) => {
                        refusals.push((
                            instance.slug().as_str().to_owned(),
                            "the search exceeded its slice of the deadline".to_owned(),
                        ));
                        continue;
                    }
                };
                if results.tracks.is_empty()
                    && results.albums.is_empty()
                    && results.artists.is_empty()
                {
                    continue;
                }
                let ids = self
                    .resolve_results(instance.id(), instance.slug(), &results)
                    .await?;
                self.store(instance.id(), text, kinds, limit, offset, &ids)
                    .await?;
                self.merge(&mut answer, &mut seen, &ids).await?;
                return Ok(answer);
            };
            self.merge(&mut answer, &mut seen, &cached).await?;
            return Ok(answer);
        }
        // No catalog instance answered and none had a cached answer: the search
        // is what the library holds alone. That reads exactly like "nothing
        // matched" in the client, so it is worth a warning naming every instance
        // that was asked and why it did not answer.
        if !refusals.is_empty() {
            tracing::warn!(
                text = %text,
                refusals = ?refusals,
                "no catalog instance answered a search; answering the library alone"
            );
        }
        Ok(answer)
    }

    /// Resolves a provider's search results into rows.
    async fn resolve_results(
        &self,
        instance: Uuid,
        slug: &hearthsound_core::Slug,
        results: &CatalogResults,
    ) -> Result<WireResults, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let mut ids = WireResults {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
        };
        for track in &results.tracks {
            let row = self
                .resolver
                .track(&mut connection, instance, slug, track)
                .await?;
            ids.tracks.push(crate::wire::WireTrack::from(
                &hearthsound_core::providers::CatalogTrack {
                    id: EntityId::from_uuid(IdPrefix::Track, row).to_string(),
                    ..track.clone()
                },
            ));
        }
        for album in &results.albums {
            let row = self
                .resolver
                .album_ref(&mut connection, instance, slug, album)
                .await?;
            ids.albums.push(crate::wire::WireAlbumRef {
                id: EntityId::from_uuid(IdPrefix::Album, row).to_string(),
                ..crate::wire::WireAlbumRef::from(album)
            });
        }
        for artist in &results.artists {
            let row = self
                .resolver
                .artist(&mut connection, instance, slug, artist)
                .await?;
            ids.artists.push(crate::wire::WireArtist {
                id: EntityId::from_uuid(IdPrefix::Artist, row).to_string(),
                ..crate::wire::WireArtist::from(artist)
            });
        }
        Ok(ids)
    }

    /// Adds resolved ids to the answer, skipping anything it already holds.
    async fn merge(
        &self,
        answer: &mut SearchAnswer,
        seen: &mut HashSet<String>,
        ids: &WireResults,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        for track in &ids.tracks {
            let Some(row) = row_of(&track.id) else {
                continue;
            };
            if !seen.insert(format!("track:{row}")) {
                continue;
            }
            let Some(track) = hearthsound_library::tracks::get_track(
                &mut connection,
                EntityId::from_uuid(IdPrefix::Track, row),
            )
            .await
            .map_err(CatalogError::Library)?
            else {
                continue;
            };
            answer.tracks.push(crate::views::TrackRef {
                artists: Vec::new(),
                album_title: None,
                has_image: false,
                track,
            });
        }
        for album in &ids.albums {
            let Some(row) = row_of(&album.id) else {
                continue;
            };
            let Some(album) = hearthsound_library::albums::get_album(
                &mut connection,
                EntityId::from_uuid(IdPrefix::Album, row),
            )
            .await
            .map_err(CatalogError::Library)?
            else {
                continue;
            };
            if !seen.insert(format!("album:{}", normalize(&album.title))) {
                continue;
            }
            answer.albums.push(AlbumRef {
                album,
                artist: None,
                has_image: false,
            });
        }
        for artist in &ids.artists {
            let Some(row) = row_of(&artist.id) else {
                continue;
            };
            let Some(artist) = hearthsound_library::artists::get_artist(
                &mut connection,
                EntityId::from_uuid(IdPrefix::Artist, row),
            )
            .await
            .map_err(CatalogError::Library)?
            else {
                continue;
            };
            if !seen.insert(format!("artist:{}", normalize(&artist.name))) {
                continue;
            }
            answer.artists.push(ArtistRef {
                artist,
                has_image: false,
            });
        }
        Ok(())
    }

    /// Reads a stored answer, when one is still fresh.
    async fn cached(
        &self,
        instance: Uuid,
        text: &str,
        kinds: &[EntityKind],
        limit: u32,
        offset: u32,
    ) -> Result<Option<WireResults>, CatalogError> {
        let key = cache_key(text, kinds, limit, offset);
        let mut connection = self.database.writer().acquire().await?;
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT entity_refs, expires_at FROM answer_cache \
             WHERE instance_id = ? AND request_key = ?",
        )
        .bind(instance.as_bytes().as_slice())
        .bind(&key)
        .fetch_optional(&mut *connection)
        .await?;
        let Some((refs, expires_at)) = row else {
            return Ok(None);
        };
        if expires_at < crate::index::unix_millis() {
            return Ok(None);
        }
        Ok(serde_json::from_str(&refs).ok())
    }

    /// Stores a resolved answer as ids.
    async fn store(
        &self,
        instance: Uuid,
        text: &str,
        kinds: &[EntityKind],
        limit: u32,
        offset: u32,
        ids: &WireResults,
    ) -> Result<(), CatalogError> {
        let key = cache_key(text, kinds, limit, offset);
        let refs = serde_json::to_string(ids)?;
        let now = crate::index::unix_millis();
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query(
            "INSERT INTO answer_cache (instance_id, request_key, entity_refs, fetched_at, expires_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (instance_id, request_key) DO UPDATE SET \
                 entity_refs = excluded.entity_refs, fetched_at = excluded.fetched_at, \
                 expires_at = excluded.expires_at",
        )
        .bind(instance.as_bytes().as_slice())
        .bind(&key)
        .bind(refs)
        .bind(now)
        .bind(now + ANSWER_TTL.as_millis() as i64)
        .execute(&mut *connection)
        .await?;
        Ok(())
    }
}

/// Returns the row a stored id names, when it is a row id.
fn row_of(id: &str) -> Option<Uuid> {
    let parsed = EntityId::parse(id).ok()?;
    Some(parsed.uuid())
}

/// Returns the cache key of a search request.
fn cache_key(text: &str, kinds: &[EntityKind], limit: u32, offset: u32) -> String {
    let mut kinds: Vec<&str> = kinds
        .iter()
        .map(|kind| match kind {
            EntityKind::Track => "track",
            EntityKind::Album => "album",
            EntityKind::Artist => "artist",
            EntityKind::Playlist => "playlist",
        })
        .collect();
    kinds.sort_unstable();
    format!(
        "search|{}|{}|{limit}|{offset}",
        normalize(text),
        kinds.join(",")
    )
}
