//! The background jobs of the entity index.
//!
//! Three kinds, and nothing schedules the indexer itself: it is a worker that
//! claims the queue ([`hearthsound_catalog::Indexer::run`]). These jobs are what
//! keeps the queue supplied and the stored data bounded:
//!
//! - `index.sweep` runs every half hour: it queues the library entities that were
//!   never deep-indexed and the ones whose data is older than
//!   `index.refresh_days`, and resets rows that failed long ago.
//! - `index.prune` runs once a day: it drops the heavy data of unviewed remote
//!   entities older than `index.prune_after_days`, deletes unreferenced ones when
//!   `index.delete_unreferenced_after_days` asks for it, sweeps orphaned image
//!   blobs and expired answers.
//! - `index.recompute` has no schedule: a settings or layer change enqueues it,
//!   and it recomputes every entity that holds provider answers from what is
//!   already stored, without calling a provider.

use std::sync::Arc;

use async_trait::async_trait;
use hearthsound_catalog::fields::{self, Precedence};
use hearthsound_catalog::{CatalogError, CatalogService, LayerOrder};
use hearthsound_config::keys;
use hearthsound_core::EntityKind;
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler};
use hearthsound_library::index_queue::{self, Depth};

/// The job kind that queues stale library entities.
pub const INDEX_SWEEP_JOB_KIND: &str = "index.sweep";

/// The cron expression of the sweep: every half hour.
pub const INDEX_SWEEP_CRON: &str = "*/30 * * * *";

/// The job kind that prunes heavy data and sweeps what it orphans.
pub const INDEX_PRUNE_JOB_KIND: &str = "index.prune";

/// The cron expression of the prune: once a day, at 04:17.
pub const INDEX_PRUNE_CRON: &str = "17 4 * * *";

/// The job kind that recomputes stored fields after a settings or layer change.
pub const INDEX_RECOMPUTE_JOB_KIND: &str = "index.recompute";

/// The most entities one sweep queues per kind.
const SWEEP_BATCH: i64 = 500;

/// Queues the library entities whose stored data is missing or stale.
pub struct IndexSweepJobHandler {
    database: Arc<Database>,
    catalog: Arc<CatalogService>,
}

impl IndexSweepJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `catalog`: the entity layer, used for the settings the sweep reads.
    pub fn new(database: Arc<Database>, catalog: Arc<CatalogService>) -> Self {
        Self { database, catalog }
    }
}

#[async_trait]
impl JobHandler for IndexSweepJobHandler {
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let settings = self.catalog.settings().current();
        let refresh_days = settings.get(keys::INDEX_REFRESH_DAYS).unwrap_or(30).max(1);
        let cutoff = unix_millis() - refresh_days * 86_400_000;
        let mut queued = 0usize;
        for kind in [EntityKind::Track, EntityKind::Album, EntityKind::Artist] {
            let mut connection = self
                .database
                .writer()
                .acquire()
                .await
                .map_err(|error| JobError::new(error.to_string()))?;
            let candidates =
                index_queue::library_candidates(&mut connection, kind, cutoff, SWEEP_BATCH)
                    .await
                    .map_err(|error| JobError::new(error.to_string()))?;
            for entity in candidates {
                index_queue::enqueue(
                    &mut connection,
                    kind,
                    entity,
                    Depth::Deep,
                    index_queue::PRIORITY_SWEEP,
                )
                .await
                .map_err(|error| JobError::new(error.to_string()))?;
                queued += 1;
            }
        }
        let mut connection = self
            .database
            .writer()
            .acquire()
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        let reset = index_queue::reset_failed(&mut connection, cutoff)
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        tracing::debug!(kind = %job.kind, queued, reset, "swept the index queue");
        Ok(())
    }
}

/// Prunes heavy data and sweeps what it orphans.
pub struct IndexPruneJobHandler {
    database: Arc<Database>,
    catalog: Arc<CatalogService>,
}

impl IndexPruneJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `catalog`: the entity layer, whose image store is swept.
    pub fn new(database: Arc<Database>, catalog: Arc<CatalogService>) -> Self {
        Self { database, catalog }
    }
}

#[async_trait]
impl JobHandler for IndexPruneJobHandler {
    async fn handle(&self, _job: Job) -> Result<(), JobError> {
        let settings = self.catalog.settings().current();
        let prune_days = settings
            .get(keys::INDEX_PRUNE_AFTER_DAYS)
            .unwrap_or(90)
            .max(0);
        let delete_days = settings
            .get(keys::INDEX_DELETE_UNREFERENCED_AFTER_DAYS)
            .unwrap_or(0)
            .max(0);
        let now = unix_millis();
        let mut connection = self
            .database
            .writer()
            .acquire()
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        if prune_days > 0 {
            let cutoff = now - prune_days * 86_400_000;
            let pruned = prune_heavy(&mut connection, cutoff)
                .await
                .map_err(|error| JobError::new(error.to_string()))?;
            tracing::debug!(pruned, "pruned heavy data of unviewed remote entities");
        }
        if delete_days > 0 {
            let cutoff = now - delete_days * 86_400_000;
            let deleted = delete_unreferenced(&mut connection, cutoff)
                .await
                .map_err(|error| JobError::new(error.to_string()))?;
            tracing::debug!(deleted, "deleted unreferenced remote entities");
        }
        let swept = self
            .catalog
            .images()
            .store()
            .sweep_orphans(&mut connection)
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        let answers = sqlx::query("DELETE FROM answer_cache WHERE expires_at < ?")
            .bind(now)
            .execute(&mut *connection)
            .await
            .map_err(|error| JobError::new(error.to_string()))?
            .rows_affected();
        tracing::debug!(swept, answers, "swept orphaned images and expired answers");
        Ok(())
    }
}

/// Recomputes stored fields after a settings or layer change.
pub struct IndexRecomputeJobHandler {
    database: Arc<Database>,
    catalog: Arc<CatalogService>,
}

impl IndexRecomputeJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `catalog`: the entity layer, used for the precedence and the layer order.
    pub fn new(database: Arc<Database>, catalog: Arc<CatalogService>) -> Self {
        Self { database, catalog }
    }
}

#[async_trait]
impl JobHandler for IndexRecomputeJobHandler {
    async fn handle(&self, _job: Job) -> Result<(), JobError> {
        let settings = self.catalog.settings().current();
        let precedence = Precedence::parse(
            &settings
                .get(keys::METADATA_PRECEDENCE)
                .unwrap_or_else(|_| "tags".to_owned()),
        );
        let order = layer_order(self.catalog.registry());
        let done = fields::recompute_all(&self.database, precedence, &order)
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        tracing::debug!(done, "recomputed stored entity fields");
        Ok(())
    }
}

/// Reads the administrator's layer order from the registry.
///
/// # Parameters
///
/// - `registry`: the provider registry.
pub fn layer_order(registry: &hearthsound_plugin_host::ProviderRegistry) -> LayerOrder {
    let ids = |layer: hearthsound_core::Layer| {
        registry
            .layers(layer)
            .into_iter()
            .map(|instance| instance.id())
            .collect()
    };
    LayerOrder {
        catalog: ids(hearthsound_core::Layer::Catalog),
        metadata: ids(hearthsound_core::Layer::Metadata),
        artwork: ids(hearthsound_core::Layer::Artwork),
        discovery: ids(hearthsound_core::Layer::Discovery),
    }
}

/// Drops the images and provider answers of unviewed remote entities.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `cutoff`: entities last viewed before this instant are pruned.
///
/// # Errors
///
/// Returns [`CatalogError`] when a statement fails.
async fn prune_heavy(conn: &mut sqlx::SqliteConnection, cutoff: i64) -> Result<u64, CatalogError> {
    let mut pruned = 0;
    for kind in ["album", "artist"] {
        let ids: Vec<Vec<u8>> = match kind {
            "album" => {
                sqlx::query_scalar(
                    "SELECT a.id FROM albums a \
                      WHERE a.fetched_at IS NOT NULL \
                        AND COALESCE(a.viewed_at, a.created_at) < ? \
                        AND NOT EXISTS (SELECT 1 FROM user_library ul \
                                        WHERE ul.entity_kind = 'album' AND ul.entity_id = a.id) \
                        AND NOT EXISTS (SELECT 1 FROM user_feedback uf \
                                        WHERE uf.entity_kind = 'album' AND uf.entity_id = a.id) \
                        AND NOT EXISTS (SELECT 1 FROM user_ratings ur \
                                        WHERE ur.entity_kind = 'album' AND ur.entity_id = a.id)",
                )
                .bind(cutoff)
                .fetch_all(&mut *conn)
                .await?
            }
            _ => {
                sqlx::query_scalar(
                    "SELECT ar.id FROM artists ar \
                      WHERE ar.fetched_at IS NOT NULL \
                        AND COALESCE(ar.viewed_at, ar.created_at) < ? \
                        AND NOT EXISTS (SELECT 1 FROM user_library ul \
                                        WHERE ul.entity_kind = 'artist' AND ul.entity_id = ar.id) \
                        AND NOT EXISTS (SELECT 1 FROM user_feedback uf \
                                        WHERE uf.entity_kind = 'artist' AND uf.entity_id = ar.id) \
                        AND NOT EXISTS (SELECT 1 FROM user_ratings ur \
                                        WHERE ur.entity_kind = 'artist' AND ur.entity_id = ar.id)",
                )
                .bind(cutoff)
                .fetch_all(&mut *conn)
                .await?
            }
        };
        for id in ids {
            if id.len() != 16 {
                continue;
            }
            pruned +=
                sqlx::query("DELETE FROM entity_sources WHERE entity_kind = ? AND entity_id = ?")
                    .bind(kind)
                    .bind(id.as_slice())
                    .execute(&mut *conn)
                    .await?
                    .rows_affected();
            sqlx::query(
                "DELETE FROM entity_images WHERE entity_kind = ? AND entity_id = ? \
                 AND instance_id IS NOT NULL",
            )
            .bind(kind)
            .bind(id.as_slice())
            .execute(&mut *conn)
            .await?;
            match kind {
                "album" => {
                    sqlx::query(
                        "UPDATE albums SET fetched_at = NULL, page_fetched_at = NULL WHERE id = ?",
                    )
                    .bind(id.as_slice())
                    .execute(&mut *conn)
                    .await?;
                }
                _ => {
                    sqlx::query(
                        "UPDATE artists SET fetched_at = NULL, page_fetched_at = NULL WHERE id = ?",
                    )
                    .bind(id.as_slice())
                    .execute(&mut *conn)
                    .await?;
                }
            }
        }
    }
    Ok(pruned)
}

/// Deletes remote entities no user, playlist, play or acquisition references.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `cutoff`: entities last viewed before this instant are candidates.
///
/// # Errors
///
/// Returns [`CatalogError`] when a statement fails.
async fn delete_unreferenced(
    conn: &mut sqlx::SqliteConnection,
    cutoff: i64,
) -> Result<u64, CatalogError> {
    let albums: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT a.id FROM albums a \
          WHERE COALESCE(a.viewed_at, a.created_at) < ? \
            AND NOT EXISTS (SELECT 1 FROM tracks t JOIN media_files mf ON mf.track_id = t.id \
                             WHERE t.album_id = a.id) \
            AND NOT EXISTS (SELECT 1 FROM acquisitions q WHERE q.album_id = a.id) \
            AND NOT EXISTS (SELECT 1 FROM user_library ul \
                             WHERE ul.entity_kind = 'album' AND ul.entity_id = a.id)",
    )
    .bind(cutoff)
    .fetch_all(&mut *conn)
    .await?;
    let mut deleted = 0u64;
    for album in albums {
        if album.len() != 16 {
            continue;
        }
        sqlx::query("DELETE FROM tracks WHERE album_id = ? AND NOT EXISTS (SELECT 1 FROM media_files mf WHERE mf.track_id = tracks.id)")
            .bind(album.as_slice())
            .execute(&mut *conn)
            .await?;
        sqlx::query("DELETE FROM albums WHERE id = ?")
            .bind(album.as_slice())
            .execute(&mut *conn)
            .await?;
        deleted += 1;
    }
    let artists: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT ar.id FROM artists ar \
          WHERE COALESCE(ar.viewed_at, ar.created_at) < ? \
            AND NOT EXISTS (SELECT 1 FROM album_artists aa WHERE aa.artist_id = ar.id) \
            AND NOT EXISTS (SELECT 1 FROM track_artists ta WHERE ta.artist_id = ar.id) \
            AND NOT EXISTS (SELECT 1 FROM user_library ul \
                             WHERE ul.entity_kind = 'artist' AND ul.entity_id = ar.id)",
    )
    .bind(cutoff)
    .fetch_all(&mut *conn)
    .await?;
    for artist in artists {
        if artist.len() != 16 {
            continue;
        }
        sqlx::query("DELETE FROM artists WHERE id = ?")
            .bind(artist.as_slice())
            .execute(&mut *conn)
            .await?;
        deleted += 1;
    }
    Ok(deleted)
}

/// Returns the current time in unix milliseconds.
fn unix_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
