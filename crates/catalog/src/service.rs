//! The catalog service: the one handle the rest of the server holds.
//!
//! It owns the pieces of the entity layer - the resolver, the indexer, the image
//! service, the views and the search - and hands each out to the caller that
//! needs it, so no crate above this one builds its own copy of any of them.

use std::sync::Arc;

use hearthsound_core::{EntityId, EntityKind, Layer};
use hearthsound_db::Database;
use hearthsound_library::index_queue::Depth;
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::ProviderRegistry;
use uuid::Uuid;

use crate::error::CatalogError;
use crate::images::ImageService;
use crate::index::Indexer;
use crate::resolve::EntityResolver;
use crate::search::{SearchAnswer, SearchService};
use crate::views::{EntityViews, Viewer};

/// The entity layer of the server.
pub struct CatalogService {
    registry: Arc<ProviderRegistry>,
    database: Arc<Database>,
    settings: hearthsound_config::settings::Settings,
    resolver: EntityResolver,
    indexer: Arc<Indexer>,
    images: Arc<ImageService>,
    views: EntityViews,
    search: SearchService,
    queue: std::sync::OnceLock<Arc<hearthsound_jobs::JobQueue>>,
}

impl CatalogService {
    /// Builds the entity layer over the provider registry.
    ///
    /// # Parameters
    ///
    /// - `registry`: the provider registry.
    /// - `database`: the open database.
    /// - `settings`: the settings store.
    /// - `artwork_root`: the directory images are stored under.
    /// - `ffmpeg`: the runner the thumbnail cache scales with.
    pub fn new(
        registry: Arc<ProviderRegistry>,
        database: Arc<Database>,
        settings: hearthsound_config::settings::Settings,
        events: Arc<hearthsound_core::events::EventBus>,
        artwork_root: impl Into<std::path::PathBuf>,
        ffmpeg: Arc<FfmpegRunner>,
    ) -> Arc<CatalogService> {
        let root = artwork_root.into();
        let images = Arc::new(ImageService::new(
            Arc::clone(&registry),
            Arc::clone(&database),
            root.clone(),
            ffmpeg,
        ));
        let indexer = Arc::new(Indexer::new(
            Arc::clone(&database),
            Arc::clone(&registry),
            settings.clone(),
            Arc::clone(&events),
            Arc::clone(&images),
            root,
        ));
        images.attach_indexer(Arc::clone(&indexer));
        let resolver = indexer.resolver().clone();
        let views = EntityViews::new(
            Arc::clone(&database),
            Arc::clone(&indexer),
            settings.clone(),
        );
        let search = SearchService::new(
            Arc::clone(&database),
            Arc::clone(&registry),
            settings.clone(),
            resolver.clone(),
        );
        Arc::new(CatalogService {
            registry,
            database,
            settings,
            resolver,
            indexer,
            images,
            views,
            search,
            queue: std::sync::OnceLock::new(),
        })
    }

    /// Gives the service the job queue a recomputation is enqueued on.
    ///
    /// # Parameters
    ///
    /// - `queue`: the server's job queue.
    pub fn attach_queue(&self, queue: Arc<hearthsound_jobs::JobQueue>) {
        let _ = self.queue.set(queue);
    }

    /// Queues the field recomputation a settings or layer change needs.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the queue refuses the job.
    pub async fn request_recompute(&self) -> Result<(), CatalogError> {
        let Some(queue) = self.queue.get() else {
            return Ok(());
        };
        queue
            .enqueue(hearthsound_jobs::NewJob::new(
                crate::index::RECOMPUTE_JOB_KIND,
            ))
            .await
            .map(|_| ())
            .map_err(CatalogError::Queue)
    }

    /// Returns the provider registry.
    pub fn registry(&self) -> &Arc<ProviderRegistry> {
        &self.registry
    }

    /// Returns the open database.
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    /// Returns the settings store.
    pub fn settings(&self) -> &hearthsound_config::settings::Settings {
        &self.settings
    }

    /// Returns the resolver every provider record enters through.
    pub fn resolver(&self) -> &EntityResolver {
        &self.resolver
    }

    /// Returns the indexer.
    pub fn indexer(&self) -> &Arc<Indexer> {
        &self.indexer
    }

    /// Returns the image service.
    pub fn images(&self) -> &Arc<ImageService> {
        &self.images
    }

    /// Returns the composed views.
    pub fn views(&self) -> &EntityViews {
        &self.views
    }

    /// Searches the library and the catalog layer.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener whose library is searched.
    /// - `text`: the query text.
    /// - `kinds`: the entity kinds to return.
    /// - `limit`: the most hits per kind.
    /// - `offset`: how many hits per kind to skip.
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
        self.search.search(user, text, kinds, limit, offset).await
    }

    /// Resolves an id through the redirects a merge leaves.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the id a client sent.
    ///
    /// # Returns
    ///
    /// The row that stands for it now, or `None` when there is none.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the read fails.
    pub async fn canonical(
        &self,
        kind: EntityKind,
        id: EntityId,
    ) -> Result<Option<EntityId>, CatalogError> {
        let prefix = match kind {
            EntityKind::Track => hearthsound_core::IdPrefix::Track,
            EntityKind::Album => hearthsound_core::IdPrefix::Album,
            EntityKind::Artist => hearthsound_core::IdPrefix::Artist,
            EntityKind::Playlist => hearthsound_core::IdPrefix::Playlist,
        };
        let mut connection = self.database.writer().acquire().await?;
        Ok(
            hearthsound_library::entities::canonical(&mut connection, kind, id.uuid())
                .await
                .map_err(CatalogError::Library)?
                .map(|row| EntityId::from_uuid(prefix, row)),
        )
    }

    /// Queues an entity for a refresh.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the row.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the write fails.
    pub async fn refresh(&self, kind: EntityKind, id: EntityId) -> Result<(), CatalogError> {
        let depth = if self.is_library_entity(kind, id.uuid()).await? {
            Depth::Deep
        } else {
            Depth::Page
        };
        let mut connection = self.database.writer().acquire().await?;
        hearthsound_library::index_queue::enqueue(&mut connection, kind, id.uuid(), depth, 0)
            .await
            .map_err(CatalogError::Library)?;
        Ok(())
    }

    /// Walks an album's page inline, for an import that needs its tracklist.
    ///
    /// # Parameters
    ///
    /// - `album`: the album's id.
    /// - `deadline`: the longest this may take.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the walk fails.
    pub async fn ensure_album_page(
        &self,
        album: EntityId,
        deadline: std::time::Duration,
    ) -> Result<(), CatalogError> {
        self.indexer
            .ensure(EntityKind::Album, album.uuid(), Depth::Page, deadline)
            .await?;
        Ok(())
    }

    /// Drops the answers an instance gave, after its configuration changed.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance's id.
    ///
    /// # Returns
    ///
    /// How many stored answers were dropped.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the write fails.
    pub async fn invalidate_instance(&self, instance: Uuid) -> Result<u64, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let affected = sqlx::query("DELETE FROM answer_cache WHERE instance_id = ?")
            .bind(instance.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?
            .rows_affected();
        Ok(affected)
    }

    /// Returns the views a page composes against.
    ///
    /// # Parameters
    ///
    /// - `viewer`: the listener the page is for.
    pub fn viewer(&self, viewer: Viewer) -> &EntityViews {
        let _ = viewer;
        &self.views
    }

    /// Reports whether an entity is one the library holds.
    async fn is_library_entity(&self, kind: EntityKind, id: Uuid) -> Result<bool, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let found: Option<i64> = match kind {
            EntityKind::Track => {
                sqlx::query_scalar(
                    "SELECT 1 AS \"found: i64\" FROM tracks t WHERE t.id = ? AND ( \
                     EXISTS (SELECT 1 FROM media_files mf WHERE mf.track_id = t.id \
                              AND mf.storage IN ('folder', 'library')) \
                     OR EXISTS (SELECT 1 FROM user_library ul WHERE ul.entity_kind = 'track' \
                                 AND ul.entity_id = t.id))",
                )
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *connection)
                .await?
            }
            EntityKind::Album => {
                sqlx::query_scalar(
                    "SELECT 1 AS \"found: i64\" FROM albums a WHERE a.id = ? AND ( \
                     EXISTS (SELECT 1 FROM tracks t JOIN media_files mf ON mf.track_id = t.id \
                              WHERE t.album_id = a.id AND mf.storage IN ('folder', 'library')) \
                     OR EXISTS (SELECT 1 FROM user_library ul WHERE ul.entity_kind = 'album' \
                                 AND ul.entity_id = a.id))",
                )
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *connection)
                .await?
            }
            _ => None,
        };
        Ok(found.is_some())
    }

    /// Returns the layers this service serves, which is the entity model's.
    pub fn layers() -> [Layer; 3] {
        [Layer::Catalog, Layer::Metadata, Layer::Artwork]
    }
}
