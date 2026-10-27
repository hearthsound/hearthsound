//! The indexer: it walks the queue and stores what the providers say.
//!
//! One worker processes one entity at a time. For an entity it calls the enabled
//! instances in the administrator's order - a catalog's record, a metadata
//! provider's release, an artwork provider's images, a discovery provider's
//! similar artists - stores each answer in `entity_sources`, resolves what the
//! answer named ([`crate::resolve`]), and recomputes the entity's fields from
//! everything stored ([`crate::fields`]).
//!
//! Depth decides how far it walks: `head` fetches the entity's own fields,
//! `page` a page's worth of data (an album's tracklist, an artist's discography
//! and top tracks), and `deep` the recursion that queues everything the page
//! points at - a library track's album, an album's credits, a primary artist's
//! discography. A featured artist is only ever indexed to `head`.
//!
//! A viewer that asks for a page whose data the server does not have runs
//! [`Indexer::ensure`], which walks the entity inline under the request's
//! deadline and reports whether it finished. Work that outlives the deadline
//! keeps going and publishes `entity.updated` when it lands.
//!
//! A walk of one entity happens once at a time however many callers ask for it:
//! the queue's worker and every viewer share one walk per entity, and every
//! caller is told how it ended. Every outcome publishes `entity.updated`, so a
//! page that was waiting renders whatever is stored - a failed walk must not
//! leave a client on a placeholder forever.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hearthsound_config::keys;
use hearthsound_core::Layer;
use hearthsound_core::ProviderError;
use hearthsound_core::events::EntityUpdated;
use hearthsound_core::events::{Audience, EventBus, IndexProgress, ServerEvent};
use hearthsound_core::providers::{
    CatalogAlbumRef, CatalogProvider, ExternalId, MB_ARTIST, MB_RECORDING, MB_RELEASE,
    MB_RELEASE_GROUP, UPC, provider_namespace,
};
use hearthsound_core::{EntityKind, Slug};
use hearthsound_db::Database;
use hearthsound_jobs::Shutdown;
use hearthsound_library::index_queue::{self, Depth, QueueItem};
use hearthsound_library::{entities, external_ids};
use hearthsound_plugin_host::ProviderRegistry;
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

use crate::error::CatalogError;
use crate::fields::{self, LayerOrder, Precedence};
use crate::images::ImageService;
use crate::resolve::EntityResolver;
use crate::sources::{self, Operation, Status};
use crate::wire::{WireAlbumRefs, WireProviderIds};

/// The job kind that recomputes stored fields after a settings or layer change.
pub const RECOMPUTE_JOB_KIND: &str = "index.recompute";

/// Whether an inline walk finished inside its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    /// Everything the depth asks for is stored.
    Complete,
    /// The walk ended without completing - it was throttled, or it failed - and
    /// what is stored is all there is until the queue tries again.
    Failed,
    /// The deadline passed; the rest is still being fetched.
    Partial,
}

/// How one walk of an entity ended.
///
/// The walk runs once per entity whichever caller started it, so its ending is
/// shared: the queue's worker records it on the row, and a viewer turns it into
/// [`Completeness`].
#[derive(Clone, Debug)]
enum WalkOutcome {
    /// Everything the depth asks for is stored.
    Done,
    /// A provider asked the caller to come back later.
    Throttled {
        /// How long the provider asked to be left alone.
        retry_after_ms: u64,
    },
    /// The walk failed for any other reason.
    Failed(String),
}

impl WalkOutcome {
    /// Condenses a walk's result.
    ///
    /// # Parameters
    ///
    /// - `outcome`: what the walk returned.
    fn of(outcome: Result<(), CatalogError>) -> WalkOutcome {
        match outcome {
            Ok(()) => WalkOutcome::Done,
            Err(error) => match error.retry_after_ms() {
                Some(retry_after_ms) => WalkOutcome::Throttled { retry_after_ms },
                None => WalkOutcome::Failed(error.to_string()),
            },
        }
    }
}

/// Waits for one walk to report how it ended.
///
/// # Parameters
///
/// - `receiver`: the walk's outcome channel, which holds `None` until it ends.
///
/// # Returns
///
/// The outcome, or [`WalkOutcome::Failed`] when the walk ended without
/// reporting, which happens only when the task driving it panicked.
async fn wait_outcome(receiver: &mut watch::Receiver<Option<WalkOutcome>>) -> WalkOutcome {
    loop {
        if let Some(outcome) = receiver.borrow_and_update().clone() {
            return outcome;
        }
        if receiver.changed().await.is_err() {
            return WalkOutcome::Failed("the walk ended without reporting".to_owned());
        }
    }
}

/// The walks that are running, by entity.
///
/// Each entry hands every caller of one entity the ending of the single walk
/// that entity is being given.
type Walks = Arc<Mutex<HashMap<(EntityKind, Uuid), watch::Receiver<Option<WalkOutcome>>>>>;

/// The entity indexer.
pub struct Indexer {
    database: Arc<Database>,
    registry: Arc<ProviderRegistry>,
    settings: hearthsound_config::settings::Settings,
    events: Arc<EventBus>,
    resolver: EntityResolver,
    images: Arc<ImageService>,
    /// The walks that are running, by entity, so a second caller joins the
    /// first instead of walking the same entity again.
    in_flight: Walks,
    done: AtomicU64,
}

impl Indexer {
    /// Builds the indexer.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `registry`: the provider registry.
    /// - `settings`: the settings store, read on every pass so a change applies
    ///   without a restart.
    /// - `events`: the bus `entity.updated` and `index.progress` are published on.
    /// - `images`: the image service, which the resolver's store is shared with
    ///   and which stores the bytes of every image a walk finds.
    /// - `artwork_root`: the directory images are stored under.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database: Arc<Database>,
        registry: Arc<ProviderRegistry>,
        settings: hearthsound_config::settings::Settings,
        events: Arc<EventBus>,
        images: Arc<ImageService>,
        artwork_root: impl Into<std::path::PathBuf>,
    ) -> Indexer {
        Indexer {
            database,
            registry,
            settings,
            events,
            resolver: EntityResolver::new(artwork_root),
            images,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            done: AtomicU64::new(0),
        }
    }

    /// Returns the resolver every provider record enters through.
    pub fn resolver(&self) -> &EntityResolver {
        &self.resolver
    }

    /// Returns how many items this process has finished.
    ///
    /// The queue's own counts are [`Indexer::queue_counts`]. This one is what the
    /// progress card's "done" shows, and it resets with the process because it
    /// describes the process's work, not the queue's.
    pub fn progress(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Returns how many items are queued and how many are marked failed.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the queue cannot be read.
    pub async fn queue_counts(&self) -> Result<(i64, i64), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        index_queue::counts(&mut connection)
            .await
            .map_err(CatalogError::Library)
    }

    /// Runs the queue until `shutdown` is signalled.
    ///
    /// # Parameters
    ///
    /// - `shutdown`: the process-wide stop signal.
    pub async fn run(&self, shutdown: Shutdown) {
        loop {
            if shutdown.is_triggered() {
                return;
            }
            match self.claim().await {
                Ok(Some(item)) => {
                    // A viewer walking the same entity joins this walk, and a
                    // viewer that started it is joined here: one entity is
                    // walked once at a time, whatever asked for it.
                    let mut receiver = self.clone_handle().walk_shared(item.clone()).await;
                    let outcome = wait_outcome(&mut receiver).await;
                    self.finish(item, outcome).await;
                    self.publish_progress().await;
                }
                Ok(None) => {
                    if self.drains() {
                        self.publish_progress().await;
                    }
                    tokio::select! {
                        () = shutdown.wait() => return,
                        () = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not claim an index queue item");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Walks one entity inline, up to the deadline.
    ///
    /// A walk of the same entity that is already running is joined, so a page
    /// read while the queue's worker walks the entity costs no second round of
    /// provider calls.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the row's raw primary key.
    /// - `depth`: how far to walk.
    /// - `deadline`: the longest this call may take.
    ///
    /// # Returns
    ///
    /// [`Completeness::Complete`] when the walk finished,
    /// [`Completeness::Failed`] when it ended without completing, and
    /// [`Completeness::Partial`] when the deadline passed; the rest keeps
    /// running and publishes `entity.updated` when it lands.
    ///
    /// # Errors
    ///
    /// This method has no failure of its own; a walk's failure is reported as
    /// [`Completeness::Failed`] so the caller renders what is stored.
    pub async fn ensure(
        &self,
        kind: EntityKind,
        id: Uuid,
        depth: Depth,
        deadline: Duration,
    ) -> Result<Completeness, CatalogError> {
        let item = QueueItem {
            kind,
            entity: id,
            depth,
            priority: 0,
            attempts: 0,
        };
        let mut receiver = self.clone_handle().walk_shared(item).await;
        match tokio::time::timeout(deadline, wait_outcome(&mut receiver)).await {
            Ok(WalkOutcome::Done) => Ok(Completeness::Complete),
            Ok(WalkOutcome::Throttled { .. } | WalkOutcome::Failed(_)) => Ok(Completeness::Failed),
            Err(_) => Ok(Completeness::Partial),
        }
    }

    /// Returns a cheap handle sharing this indexer's state.
    fn clone_handle(&self) -> IndexerHandle {
        IndexerHandle {
            database: Arc::clone(&self.database),
            registry: Arc::clone(&self.registry),
            settings: self.settings.clone(),
            events: Arc::clone(&self.events),
            resolver: self.resolver.clone(),
            images: Arc::clone(&self.images),
            in_flight: Arc::clone(&self.in_flight),
        }
    }

    /// Claims one due row.
    async fn claim(&self) -> Result<Option<QueueItem>, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        index_queue::claim(&mut connection)
            .await
            .map_err(CatalogError::Library)
    }

    /// Reports whether this process has finished an item, which is when progress
    /// is worth publishing while the queue waits.
    fn drains(&self) -> bool {
        self.done.load(Ordering::Relaxed) > 0
    }

    /// Records the outcome of one claimed row.
    ///
    /// The walk that produced the outcome has already published
    /// `entity.updated`, so this only records what the row's next attempt needs.
    ///
    /// # Parameters
    ///
    /// - `item`: the claimed row.
    /// - `outcome`: how the walk ended.
    async fn finish(&self, item: QueueItem, outcome: WalkOutcome) {
        let Ok(mut connection) = self.database.writer().acquire().await else {
            return;
        };
        match outcome {
            WalkOutcome::Done => {
                let _ = index_queue::complete(&mut connection, &item).await;
                self.done.fetch_add(1, Ordering::Relaxed);
            }
            WalkOutcome::Throttled { retry_after_ms } => {
                let wait = i64::try_from(retry_after_ms).unwrap_or(1_000).max(1_000);
                let _ = index_queue::defer(&mut connection, &item, unix_millis() + wait).await;
            }
            WalkOutcome::Failed(message) => {
                let _ = index_queue::fail(&mut connection, &item, &message, None).await;
            }
        }
    }

    /// Publishes the queue's progress to administrators.
    async fn publish_progress(&self) {
        let Ok(mut connection) = self.database.writer().acquire().await else {
            return;
        };
        let Ok((pending, failed)) = index_queue::counts(&mut connection).await else {
            return;
        };
        let done = self.done.load(Ordering::Relaxed);
        let pending = pending.max(0) as u64;
        self.events.publish_for(
            Audience::Admins,
            ServerEvent::IndexProgress(IndexProgress {
                pending,
                failed: failed.max(0) as u64,
                done,
                total: done + pending,
            }),
        );
    }
}

/// The parts one walk needs, cloned so a spawned walk outlives its caller.
#[derive(Clone)]
struct IndexerHandle {
    database: Arc<Database>,
    registry: Arc<ProviderRegistry>,
    settings: hearthsound_config::settings::Settings,
    events: Arc<EventBus>,
    resolver: EntityResolver,
    images: Arc<ImageService>,
    in_flight: Walks,
}

impl IndexerHandle {
    /// Joins the walk of an entity, starting one when none is running.
    ///
    /// One entity is walked once at a time: the queue's worker and every viewer
    /// share the walk that is running, and each of them is told how it ended.
    /// The walk publishes `entity.updated` whatever its outcome - a page waiting
    /// on it must re-read and render what is stored rather than wait forever -
    /// and it is driven by a task of its own so it outlives a caller's deadline.
    ///
    /// # Parameters
    ///
    /// - `item`: the entity, the depth to walk and the priority it would be
    ///   queued at; a walk that a caller joins ignores this and uses the depth
    ///   the running walk started with.
    ///
    /// # Returns
    ///
    /// A receiver that holds `None` until the walk ends and then its
    /// [`WalkOutcome`].
    async fn walk_shared(&self, item: QueueItem) -> watch::Receiver<Option<WalkOutcome>> {
        let key = (item.kind, item.entity);
        let mut in_flight = self.in_flight.lock().await;
        if let Some(receiver) = in_flight.get(&key) {
            return receiver.clone();
        }
        let (sender, receiver) = watch::channel(None);
        in_flight.insert(key, receiver.clone());
        drop(in_flight);

        let walker = self.clone();
        let publisher = self.clone();
        let entity = item.entity;
        let kind = item.kind;
        tokio::spawn(async move {
            // A panic in the walk must not leave the entity un-walkable: the
            // inner task reports it, so the entry is always removed and every
            // waiter is answered.
            let outcome = match tokio::spawn(async move { walker.process(&item).await }).await {
                Ok(result) => WalkOutcome::of(result),
                Err(error) => WalkOutcome::Failed(format!("the walk panicked: {error}")),
            };
            if let WalkOutcome::Failed(message) = &outcome {
                tracing::debug!(?kind, entity = %entity, %message, "an index walk failed");
            }
            publisher.in_flight.lock().await.remove(&key);
            publisher.publish_updated(kind, entity);
            let _ = sender.send(Some(outcome));
        });
        receiver
    }

    /// Publishes `entity.updated` for a stored change.
    fn publish_updated(&self, kind: EntityKind, id: Uuid) {
        publish_updated(&self.events, kind, id);
    }

    /// Walks one entity through its depth steps.
    async fn process(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let order = self.order();
        match (item.kind, item.depth) {
            (EntityKind::Track, Depth::Head) | (EntityKind::Track, Depth::Page) => {
                self.track_head(item, &order).await
            }
            (EntityKind::Track, Depth::Deep) => self.track_deep(item, &order).await,
            (EntityKind::Album, Depth::Head) => self.album_head(item, &order).await,
            (EntityKind::Album, Depth::Page) => self.album_page(item, &order).await,
            (EntityKind::Album, Depth::Deep) => self.album_deep(item, &order).await,
            (EntityKind::Artist, Depth::Head) => self.artist_head(item, &order).await,
            (EntityKind::Artist, Depth::Page) => self.artist_page(item, &order).await,
            (EntityKind::Artist, Depth::Deep) => self.artist_deep(item, &order).await,
            (EntityKind::Playlist, _) => Ok(()),
        }
    }

    /// Returns the layer order the field rule uses.
    fn order(&self) -> LayerOrder {
        let ids = |layer: Layer| -> Vec<Uuid> {
            self.registry
                .layers(layer)
                .into_iter()
                .map(|instance| instance.id())
                .collect()
        };
        LayerOrder {
            catalog: ids(Layer::Catalog),
            metadata: ids(Layer::Metadata),
            artwork: ids(Layer::Artwork),
            discovery: ids(Layer::Discovery),
        }
    }

    /// Recomputes an entity's fields after a pass.
    async fn recompute(&self, kind: EntityKind, id: Uuid) -> Result<(), CatalogError> {
        let precedence = Precedence::parse(
            &self
                .settings
                .current()
                .get(keys::METADATA_PRECEDENCE)
                .unwrap_or_else(|_| "tags".to_owned()),
        );
        let order = self.order();
        let mut connection = self.database.writer().acquire().await?;
        fields::recompute(&mut connection, kind, id, precedence, &order).await?;
        Ok(())
    }

    /// Indexes a track's own fields.
    async fn track_head(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        let ids = self.ids_of(EntityKind::Track, item.entity).await?;
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            match retry_throttled(|| provider.track(&provider_id)).await {
                Ok(track) => {
                    self.store_catalog_track(item, instance.id(), instance.slug(), &track)
                        .await?;
                }
                Err(ProviderError::NotFound) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Catalog,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) if throttled(&error) => {
                    return Err(CatalogError::from_provider(instance.slug().as_str(), error));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a catalog instance could not answer while indexing"
                    );
                }
            }
        }
        // A library track with no identifier gets one from the metadata layer,
        // which is what lets a second provider agree on the same recording.
        if !ids.iter().any(|id| id.namespace == MB_RECORDING) {
            self.resolve_library_track(item).await?;
        }
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE tracks SET fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        let _ = order;
        self.recompute(EntityKind::Track, item.entity).await
    }

    /// Indexes a track, queues its album, and queues its credited artists.
    ///
    /// Only an album or an artist that has never been indexed is queued: a
    /// second track of the same album asked for the same work, and the queue's
    /// depth is a floor rather than a schedule. The artists a library track
    /// credits are held artists, so their pages are queued too, which is what
    /// keeps every page a held track reaches stored instead of fetched while it
    /// is viewed.
    async fn track_deep(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.track_head(item, order).await?;
        let mut connection = self.database.writer().acquire().await?;
        let album: Option<Option<Vec<u8>>> =
            sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?")
                .bind(item.entity.as_bytes().as_slice())
                .fetch_optional(&mut *connection)
                .await?;
        sqlx::query("UPDATE tracks SET deep_indexed_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        if let Some(Some(bytes)) = album
            && bytes.len() == 16
            && let Ok(album) = Uuid::from_slice(&bytes)
        {
            let indexed: Option<Option<i64>> =
                sqlx::query_scalar("SELECT deep_indexed_at FROM albums WHERE id = ?")
                    .bind(album.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?;
            if indexed.flatten().is_none() {
                index_queue::enqueue(
                    &mut connection,
                    EntityKind::Album,
                    album,
                    Depth::Deep,
                    item.priority,
                )
                .await
                .map_err(CatalogError::Library)?;
            }
        }
        let artists: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT a.id FROM track_artists ta JOIN artists a ON a.id = ta.artist_id \
             WHERE ta.track_id = ? AND a.page_fetched_at IS NULL",
        )
        .bind(item.entity.as_bytes().as_slice())
        .fetch_all(&mut *connection)
        .await?;
        for artist in artists {
            if artist.len() != 16 {
                continue;
            }
            let Ok(artist) = Uuid::from_slice(&artist) else {
                continue;
            };
            index_queue::enqueue(
                &mut connection,
                EntityKind::Artist,
                artist,
                Depth::Page,
                item.priority,
            )
            .await
            .map_err(CatalogError::Library)?;
        }
        Ok(())
    }

    /// Indexes an album's own head: the image a page shows for it.
    ///
    /// [`Depth::Head`] on an album means a page needs the album's cover and
    /// nothing else - a remote album listed on a held page, or a discography
    /// entry nobody has opened. The cover's bytes are stored here too, so the
    /// page draws it without a provider call.
    async fn album_head(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.album_images(item).await?;
        self.prefetch_image(item).await;
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE albums SET fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        let _ = order;
        self.recompute(EntityKind::Album, item.entity).await
    }

    /// Indexes an album's page: its record, tracklist, release and images.
    async fn album_page(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        let ids = self.ids_of(EntityKind::Album, item.entity).await?;
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            match retry_throttled(|| provider.album(&provider_id)).await {
                Ok(album) => {
                    self.store_catalog_album(item, instance.id(), instance.slug(), &album)
                        .await?;
                }
                Err(ProviderError::NotFound) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Catalog,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) if throttled(&error) => {
                    return Err(CatalogError::from_provider(instance.slug().as_str(), error));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a catalog instance could not answer while indexing"
                    );
                }
            }
        }
        // An album with canonical ids but no catalog record is looked up by the
        // ids themselves, which is what lets MusicBrainz name a release the
        // catalog layer does not carry.
        // One id is enough to find the release, and every id sent is one more
        // request inside the plugin: the release id first, then the release
        // group, then the barcode.
        let lookup = ids
            .iter()
            .find(|id| id.namespace == MB_RELEASE)
            .or_else(|| ids.iter().find(|id| id.namespace == MB_RELEASE_GROUP))
            .or_else(|| ids.iter().find(|id| id.namespace == UPC))
            .cloned();
        if !ids
            .iter()
            .any(|id| id.namespace.starts_with("p:") && id.value != ids[0].value)
            && let Some(lookup) = lookup
        {
            self.lookup_album(item, std::slice::from_ref(&lookup))
                .await?;
        }
        if ids
            .iter()
            .any(|id| id.namespace == MB_RELEASE || id.namespace == MB_RELEASE_GROUP)
        {
            self.album_release(item, &ids).await?;
        }
        self.album_images(item).await?;
        self.prefetch_image(item).await;
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE albums SET fetched_at = ?, page_fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        let _ = order;
        self.recompute(EntityKind::Album, item.entity).await
    }

    /// Indexes an album and queues the artists its credits name.
    async fn album_deep(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.album_page(item, order).await?;
        let mut connection = self.database.writer().acquire().await?;
        let credits: Vec<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT artist_id, role FROM album_artists WHERE album_id = ? ORDER BY position",
        )
        .bind(item.entity.as_bytes().as_slice())
        .fetch_all(&mut *connection)
        .await?;
        for (artist, role) in credits {
            if artist.len() != 16 {
                continue;
            }
            let Ok(artist) = Uuid::from_slice(&artist) else {
                continue;
            };
            // A primary artist is deep-indexed once, which is what keeps the
            // recursion from circling between artists and their albums. A
            // featured artist of a held album is a held artist, so its page is
            // stored; an artist that already has one is not queued again.
            let row: Option<(Option<i64>, Option<i64>)> =
                sqlx::query_as("SELECT deep_indexed_at, page_fetched_at FROM artists WHERE id = ?")
                    .bind(artist.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?;
            let Some((deep_indexed_at, page_fetched_at)) = row else {
                continue;
            };
            let depth = if role == "main" {
                if deep_indexed_at.is_some() {
                    continue;
                }
                Depth::Deep
            } else {
                if page_fetched_at.is_some() {
                    continue;
                }
                Depth::Page
            };
            index_queue::enqueue(
                &mut connection,
                EntityKind::Artist,
                artist,
                depth,
                item.priority,
            )
            .await
            .map_err(CatalogError::Library)?;
        }
        sqlx::query("UPDATE albums SET deep_indexed_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        Ok(())
    }

    /// Indexes an artist's own fields.
    async fn artist_head(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        let ids = self.ids_of(EntityKind::Artist, item.entity).await?;
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            match retry_throttled(|| provider.artist(&provider_id)).await {
                Ok(artist) => {
                    self.store_catalog_artist(item, instance.id(), instance.slug(), &artist)
                        .await?;
                }
                Err(ProviderError::NotFound) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Catalog,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) if throttled(&error) => {
                    return Err(CatalogError::from_provider(instance.slug().as_str(), error));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a catalog instance could not answer while indexing"
                    );
                }
            }
        }
        let name = self.artist_name(item.entity).await?;
        for instance in &self.registry.layers(Layer::Metadata) {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            let request = hearthsound_core::providers::ArtistInfoRequest {
                name: name.clone(),
                ids: ids.clone(),
            };
            match retry_throttled(|| provider.artist_info(&request)).await {
                Ok(Some(info)) => {
                    let held = mb_artist_of(&ids);
                    let named = mb_artist_of(&info.ids);
                    if let (Some(held), Some(named)) = (held, named)
                        && held != named
                    {
                        tracing::warn!(
                            held = %held,
                            named = %named,
                            instance = instance.slug().as_str(),
                            "an artist-info answer named another artist"
                        );
                        self.store_answer(
                            item,
                            instance.id(),
                            Operation::ArtistInfo,
                            Status::Missing,
                            None,
                        )
                        .await?;
                        continue;
                    }
                    let payload = serde_json::to_string(&crate::wire::WireArtistInfo::from(&info))?;
                    self.store_payload(
                        item,
                        instance.id(),
                        Operation::ArtistInfo,
                        Status::Found,
                        Some(&payload),
                    )
                    .await?;
                }
                Ok(None) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::ArtistInfo,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) if throttled(&error) => {
                    return Err(CatalogError::from_provider(instance.slug().as_str(), error));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a catalog instance could not answer while indexing"
                    );
                }
            }
        }
        self.adopt_catalog_ids(item, &name).await?;
        self.artist_images(item).await?;
        self.prefetch_image(item).await;
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE artists SET fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        let _ = order;
        self.recompute(EntityKind::Artist, item.entity).await
    }

    /// Gives an artist the identifiers of a catalog instance that knows it by
    /// a shared one.
    ///
    /// A discovery service names an artist by a shared identifier such as its
    /// MusicBrainz id, which no catalog instance owns; the catalog that can list
    /// the artist's albums answers by its own id instead. The artist is searched
    /// there by name, and a hit is adopted only when it names an identifier the
    /// artist already holds, so two artists that merely share a name are never
    /// taken for one.
    ///
    /// # Parameters
    ///
    /// - `item`: the artist's queue item.
    /// - `name`: the artist's name.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails.
    async fn adopt_catalog_ids(&self, item: &QueueItem, name: &str) -> Result<(), CatalogError> {
        if name.is_empty() {
            return Ok(());
        }
        let ids = self.ids_of(EntityKind::Artist, item.entity).await?;
        if ids.is_empty() {
            return Ok(());
        }
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            if provider_id(&ids, instance.slug()).is_some() {
                continue;
            }
            let query = hearthsound_core::providers::CatalogQuery {
                text: name.to_owned(),
                kinds: vec![EntityKind::Artist],
                limit: 5,
                offset: 0,
            };
            let Ok(results) = retry_throttled(|| provider.search(&query)).await else {
                continue;
            };
            for hit in &results.artists {
                if !hit.ids.iter().any(|named| {
                    ids.iter()
                        .any(|held| held.namespace == named.namespace && held.value == named.value)
                }) {
                    continue;
                }
                let mut connection = self.database.writer().acquire().await?;
                self.resolver
                    .artist(&mut connection, instance.id(), instance.slug(), hit)
                    .await?;
                drop(connection);
                break;
            }
        }
        Ok(())
    }

    /// Indexes an artist's page: discography, top tracks and similar artists.
    async fn artist_page(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.artist_head(item, order).await?;
        let ids = self.ids_of(EntityKind::Artist, item.entity).await?;
        let name = self.artist_name(item.entity).await?;

        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            let references = self
                .discography(item, instance.id(), instance.slug(), provider, &provider_id)
                .await?;
            if !references.is_empty() {
                break;
            }
        }
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            let tracks = match retry_throttled(|| provider.artist_top_tracks(&provider_id, 100))
                .await
            {
                Ok(tracks) if !tracks.is_empty() => tracks,
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(%error, instance = instance.slug().as_str(), "no top tracks");
                    continue;
                }
            };
            let mut resolved = Vec::with_capacity(tracks.len());
            let mut connection = self.database.writer().acquire().await?;
            for track in &tracks {
                let row = self
                    .resolver
                    .track(&mut connection, instance.id(), instance.slug(), track)
                    .await?;
                resolved.push(row);
            }
            drop(connection);
            let ids: Vec<String> = resolved.iter().map(Uuid::to_string).collect();
            let payload = serde_json::to_string(&WireProviderIds::new(ids))?;
            self.store_payload(
                item,
                instance.id(),
                Operation::CatalogTopTracks,
                Status::Found,
                Some(&payload),
            )
            .await?;
            // The shelf draws each track's cover from its album, which a remote
            // album may not have fetched yet.
            self.prefetch_top_track_albums(&resolved).await?;
            break;
        }
        self.similar_artists(item, &name, &ids).await?;
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE artists SET page_fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        self.recompute(EntityKind::Artist, item.entity).await
    }

    /// Queues the covers of the albums a page's top tracks sit on.
    ///
    /// # Parameters
    ///
    /// - `tracks`: the track rows a page will show.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the read or the write fails.
    async fn prefetch_top_track_albums(&self, tracks: &[Uuid]) -> Result<(), CatalogError> {
        if tracks.is_empty() {
            return Ok(());
        }
        let mut connection = self.database.writer().acquire().await?;
        for track in tracks {
            let album: Option<Option<Vec<u8>>> =
                sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?")
                    .bind(track.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?;
            let Some(Some(bytes)) = album else {
                continue;
            };
            if bytes.len() != 16 {
                continue;
            }
            let Ok(album) = Uuid::from_slice(&bytes) else {
                continue;
            };
            if crate::held::album(&mut connection, album).await? {
                continue;
            }
            let fetched: Option<Option<i64>> =
                sqlx::query_scalar("SELECT fetched_at FROM albums WHERE id = ?")
                    .bind(album.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?;
            if fetched.flatten().is_some() {
                continue;
            }
            index_queue::enqueue(
                &mut connection,
                EntityKind::Album,
                album,
                Depth::Head,
                PREFETCH_PRIORITY,
            )
            .await
            .map_err(CatalogError::Library)?;
        }
        Ok(())
    }

    /// Indexes an artist and queues its discography.
    async fn artist_deep(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.artist_page(item, order).await?;
        let mut connection = self.database.writer().acquire().await?;
        let albums: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT album_id FROM album_artists WHERE artist_id = ? ORDER BY position",
        )
        .bind(item.entity.as_bytes().as_slice())
        .fetch_all(&mut *connection)
        .await?;
        for album in albums {
            if album.len() != 16 {
                continue;
            }
            let Ok(album) = Uuid::from_slice(&album) else {
                continue;
            };
            let row: Option<(Option<i64>, Option<i64>)> =
                sqlx::query_as("SELECT page_fetched_at, fetched_at FROM albums WHERE id = ?")
                    .bind(album.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?;
            let Some((page_fetched_at, fetched_at)) = row else {
                continue;
            };
            if crate::held::album(&mut connection, album).await? {
                if page_fetched_at.is_some() {
                    continue;
                }
                index_queue::enqueue(
                    &mut connection,
                    EntityKind::Album,
                    album,
                    Depth::Page,
                    item.priority,
                )
                .await
                .map_err(CatalogError::Library)?;
            } else if fetched_at.is_none() {
                index_queue::enqueue(
                    &mut connection,
                    EntityKind::Album,
                    album,
                    Depth::Head,
                    PREFETCH_PRIORITY,
                )
                .await
                .map_err(CatalogError::Library)?;
            }
        }
        sqlx::query("UPDATE artists SET deep_indexed_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        Ok(())
    }

    /// Lists an artist's discography from one catalog instance.
    async fn discography(
        &self,
        item: &QueueItem,
        instance: Uuid,
        slug: &Slug,
        provider: &Arc<dyn CatalogProvider>,
        provider_id: &str,
    ) -> Result<Vec<CatalogAlbumRef>, CatalogError> {
        let mut references = Vec::new();
        let mut offset = 0u32;
        while references.len() < 500 {
            let page =
                match retry_throttled(|| provider.artist_albums(provider_id, 100, offset)).await {
                    Ok(page) => page,
                    Err(error) if throttled(&error) => {
                        return Err(CatalogError::from_provider(slug.as_str(), error));
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            instance = slug.as_str(),
                            "a catalog instance could not list a discography"
                        );
                        break;
                    }
                };
            if page.is_empty() {
                break;
            }
            let returned = page.len();
            offset += returned as u32;
            // An artist's page lists albums, EPs and singles: a compilation, a
            // live recording, a remix set and the rest are releases of those
            // albums, not works of their own.
            references.extend(
                page.into_iter()
                    .filter(|reference| is_discography_release(reference.album_type.as_deref())),
            );
            // A short page is the last one; asking again costs a request that
            // returns nothing.
            if returned < 100 {
                break;
            }
        }
        let mut connection = self.database.writer().acquire().await?;
        for reference in &references {
            let album = self
                .resolver
                .album_ref(&mut connection, instance, slug, reference)
                .await?;
            // A held album is the scan's to queue, at the depth a page needs. A
            // remote one only has to show its cover on the artist's page, so its
            // head is prefetched below the held work.
            if crate::held::album(&mut connection, album).await? {
                continue;
            }
            let fetched_at: Option<Option<i64>> =
                sqlx::query_scalar("SELECT fetched_at FROM albums WHERE id = ?")
                    .bind(album.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?;
            if fetched_at.flatten().is_none() {
                index_queue::enqueue(
                    &mut connection,
                    EntityKind::Album,
                    album,
                    Depth::Head,
                    PREFETCH_PRIORITY,
                )
                .await
                .map_err(CatalogError::Library)?;
            }
        }
        drop(connection);
        let payload = serde_json::to_string(&WireAlbumRefs::new(
            references
                .iter()
                .map(crate::wire::WireAlbumRef::from)
                .collect(),
        ))?;
        self.store_payload(
            item,
            instance,
            Operation::CatalogAlbums,
            Status::Found,
            Some(&payload),
        )
        .await?;
        Ok(references)
    }

    /// Asks the discovery layer for similar artists and stores their rows.
    ///
    /// The first instance that names anybody answers. An instance that names
    /// nobody is recorded too, but only when every instance answered: an
    /// instance that failed says nothing about the artist, and recording its
    /// failure as "no similar artists" is what left the shelf empty until the
    /// staleness sweep came round. A provider that is out of budget after its
    /// retries defers the whole pass instead.
    ///
    /// The artists named are prefetched at [`PREFETCH_PRIORITY`], because the
    /// artist's page shows them: a shelf of names with no portraits is exactly
    /// the placeholder a held page must not show.
    async fn similar_artists(
        &self,
        item: &QueueItem,
        name: &str,
        ids: &[ExternalId],
    ) -> Result<(), CatalogError> {
        let seed = hearthsound_core::providers::ArtistSeed {
            name: name.to_owned(),
            ids: ids.to_vec(),
        };
        let mut every_instance_answered = true;
        for instance in &self.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let suggestions =
                match retry_throttled(|| provider.similar_artists(None, &seed, 20)).await {
                    Ok(suggestions) if !suggestions.is_empty() => suggestions,
                    Ok(_) => continue,
                    Err(error) if throttled(&error) => {
                        return Err(CatalogError::from_provider(instance.slug().as_str(), error));
                    }
                    Err(error) => {
                        every_instance_answered = false;
                        tracing::warn!(
                            %error,
                            instance = instance.slug().as_str(),
                            "no similar artists"
                        );
                        continue;
                    }
                };
            let mut resolved = Vec::with_capacity(suggestions.len());
            let mut connection = self.database.writer().acquire().await?;
            for suggestion in &suggestions {
                let row = self
                    .resolver
                    .artist_with_ids(
                        &mut connection,
                        instance.slug(),
                        &suggestion.name,
                        &suggestion.ids,
                        None,
                    )
                    .await?;
                resolved.push(row);
            }
            drop(connection);
            let ids: Vec<String> = resolved.iter().map(Uuid::to_string).collect();
            let payload = serde_json::to_string(&WireProviderIds::new(ids))?;
            self.store_payload(
                item,
                instance.id(),
                Operation::SimilarArtists,
                Status::Found,
                Some(&payload),
            )
            .await?;
            self.prefetch_artist_heads(&resolved).await?;
            return Ok(());
        }
        if every_instance_answered
            && let Some(instance) = self.registry.layers(Layer::Discovery).first()
        {
            self.store_payload(
                item,
                instance.id(),
                Operation::SimilarArtists,
                Status::Missing,
                None,
            )
            .await?;
        }
        Ok(())
    }

    /// Queues the heads of artists whose page shows nothing yet.
    ///
    /// # Parameters
    ///
    /// - `artists`: the artist rows a page will show.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the read or the write fails.
    async fn prefetch_artist_heads(&self, artists: &[Uuid]) -> Result<(), CatalogError> {
        if artists.is_empty() {
            return Ok(());
        }
        let mut connection = self.database.writer().acquire().await?;
        for artist in artists {
            let fetched: Option<Option<i64>> =
                sqlx::query_scalar("SELECT fetched_at FROM artists WHERE id = ?")
                    .bind(artist.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?;
            if fetched.flatten().is_some() {
                continue;
            }
            index_queue::enqueue(
                &mut connection,
                EntityKind::Artist,
                *artist,
                Depth::Head,
                PREFETCH_PRIORITY,
            )
            .await
            .map_err(CatalogError::Library)?;
        }
        Ok(())
    }

    /// Asks the metadata layer to name a library track's recording.
    async fn resolve_library_track(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let row: Option<(String, Option<String>, Option<i64>)> =
            sqlx::query_as("SELECT title, display_artist, duration_ms FROM tracks WHERE id = ?")
                .bind(item.entity.as_bytes().as_slice())
                .fetch_optional(&mut *connection)
                .await?;
        drop(connection);
        let Some((title, display_artist, duration_ms)) = row else {
            return Ok(());
        };
        let hint = hearthsound_core::providers::RecordingHint {
            title: title.clone(),
            artists: display_artist.iter().cloned().collect(),
            album: None,
            duration_ms: duration_ms.and_then(|value| u32::try_from(value).ok()),
            isrc: None,
        };
        for instance in &self.registry.layers(Layer::Metadata) {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            let matches = match retry_throttled(|| provider.resolve_recording(&hint)).await {
                Ok(matches) => matches,
                Err(error) => {
                    tracing::warn!(%error, instance = instance.slug().as_str(), "no recording match");
                    continue;
                }
            };
            let Some(best) = matches.first() else {
                continue;
            };
            let mut connection = self.database.writer().acquire().await?;
            let conflicts =
                external_ids::attach(&mut connection, EntityKind::Track, item.entity, &best.ids)
                    .await
                    .map_err(CatalogError::Library)?;
            for conflict in conflicts {
                entities::merge(
                    &mut connection,
                    EntityKind::Track,
                    item.entity,
                    conflict.owner,
                )
                .await
                .map_err(CatalogError::Library)?;
            }
            break;
        }
        Ok(())
    }

    /// Looks an album up by its canonical ids on the catalog layer.
    async fn lookup_album(&self, item: &QueueItem, ids: &[ExternalId]) -> Result<(), CatalogError> {
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let references = match retry_throttled(|| provider.lookup_album(ids)).await {
                Ok(references) if !references.is_empty() => references,
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(%error, instance = instance.slug().as_str(), "no album lookup");
                    continue;
                }
            };
            let Some(first) = references.first() else {
                continue;
            };
            let mut connection = self.database.writer().acquire().await?;
            let album = self
                .resolver
                .album_ref(&mut connection, instance.id(), instance.slug(), first)
                .await?;
            if album != item.entity {
                entities::merge(&mut connection, EntityKind::Album, item.entity, album)
                    .await
                    .map_err(CatalogError::Library)?;
            }
            let stored = external_ids::ids_of(&mut connection, EntityKind::Album, item.entity)
                .await
                .map_err(CatalogError::Library)?;
            drop(connection);
            // The provider call happens on no connection: a slow service must
            // never hold the writer another request needs.
            if let Some(provider_id) = provider_id(&stored, instance.slug())
                && let Ok(album) = provider.album(&provider_id).await
            {
                self.store_catalog_album(item, instance.id(), instance.slug(), &album)
                    .await?;
            }
            break;
        }
        Ok(())
    }

    /// Asks the metadata layer for an album's release.
    ///
    /// The first instance that answers stores its release payload, which is what
    /// lets a metadata provider's title, date, type, edition and genres win the
    /// fields it fills.
    async fn album_release(
        &self,
        item: &QueueItem,
        ids: &[ExternalId],
    ) -> Result<(), CatalogError> {
        for instance in &self.registry.layers(Layer::Metadata) {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            match retry_throttled(|| provider.get_release(ids)).await {
                Ok(album) => {
                    let payload = serde_json::to_string(&crate::wire::WireAlbum::from(&album))?;
                    self.store_payload(
                        item,
                        instance.id(),
                        Operation::Release,
                        Status::Found,
                        Some(&payload),
                    )
                    .await?;
                    let mut connection = self.database.writer().acquire().await?;
                    self.resolver
                        .album(&mut connection, instance.id(), instance.slug(), &album)
                        .await?;
                    break;
                }
                Err(ProviderError::NotFound) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Release,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) if throttled(&error) => {
                    return Err(CatalogError::from_provider(instance.slug().as_str(), error));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a metadata instance could not answer for the release"
                    );
                }
            }
        }
        Ok(())
    }

    /// Stores the bytes of the image a walk named for its entity.
    ///
    /// A walk records which image an instance named; the bytes used to be
    /// downloaded on the first read, so a page whose provider was unreachable
    /// showed a placeholder forever. Downloading them here - while the instance's
    /// rate limit budget is already being spent - is what makes a prefetched page
    /// complete without the network.
    ///
    /// A failure is logged and ignored: the image row still names the URL, and a
    /// later read tries it again.
    ///
    /// # Parameters
    ///
    /// - `item`: the entity the walk stored images for.
    async fn prefetch_image(&self, item: &QueueItem) {
        if let Err(error) = self.images.primary(item.kind, item.entity).await {
            tracing::debug!(
                ?item.kind,
                entity = %item.entity,
                %error,
                "could not store an image an index pass found"
            );
        }
    }

    /// Asks the artwork layer for an album's images.
    async fn album_images(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let subject = hearthsound_core::providers::ArtworkSubject {
            kind: EntityKind::Album,
            provider_id: None,
            title: self.title_of(item.kind, item.entity).await?,
            ids: self.ids_of(EntityKind::Album, item.entity).await?,
            artists: Vec::new(),
        };
        self.artwork(item, subject).await
    }

    /// Asks the artwork layer for an artist's images.
    async fn artist_images(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let subject = hearthsound_core::providers::ArtworkSubject {
            kind: EntityKind::Artist,
            provider_id: None,
            title: self.title_of(item.kind, item.entity).await?,
            ids: self.ids_of(EntityKind::Artist, item.entity).await?,
            artists: Vec::new(),
        };
        self.artwork(item, subject).await
    }

    /// Stores what each artwork instance names for a subject.
    async fn artwork(
        &self,
        item: &QueueItem,
        subject: hearthsound_core::providers::ArtworkSubject,
    ) -> Result<(), CatalogError> {
        for instance in &self.registry.layers(Layer::Artwork) {
            let Some(provider) = instance.artwork() else {
                continue;
            };
            match retry_throttled(|| provider.find_artwork(&subject)).await {
                Ok(images) if images.is_empty() => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Artwork,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Ok(images) => {
                    let payload = serde_json::to_string(&crate::wire::WireImages::new(
                        images.iter().map(crate::wire::WireImage::from).collect(),
                    ))?;
                    self.store_payload(
                        item,
                        instance.id(),
                        Operation::Artwork,
                        Status::Found,
                        Some(&payload),
                    )
                    .await?;
                    let role = if subject.kind == EntityKind::Artist {
                        "portrait"
                    } else {
                        "cover"
                    };
                    let mut connection = self.database.writer().acquire().await?;
                    let images = images.clone();
                    let item_kind = item.kind;
                    let item_entity = item.entity;
                    let instance_id = instance.id();
                    self.resolver
                        .store_provider_images(
                            &mut connection,
                            item_kind,
                            item_entity,
                            instance_id,
                            role,
                            &images,
                        )
                        .await?;
                }
                Err(error) if throttled(&error) => {
                    return Err(CatalogError::from_provider(instance.slug().as_str(), error));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "an artwork instance could not answer"
                    );
                }
            }
        }
        Ok(())
    }

    /// Stores a catalog track's record and every row it names.
    async fn store_catalog_track(
        &self,
        item: &QueueItem,
        instance: Uuid,
        slug: &Slug,
        track: &hearthsound_core::providers::CatalogTrack,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        if let Some(album) = &track.album {
            let album_row = self
                .resolver
                .album_ref(&mut connection, instance, slug, album)
                .await?;
            let mut owned = track.clone();
            owned.album = None;
            let row = self
                .resolver
                .track(&mut connection, instance, slug, &owned)
                .await?;
            sqlx::query("UPDATE tracks SET album_id = COALESCE(album_id, ?) WHERE id = ?")
                .bind(album_row.as_bytes().as_slice())
                .bind(row.as_bytes().as_slice())
                .execute(&mut *connection)
                .await?;
        } else {
            self.resolver
                .track(&mut connection, instance, slug, track)
                .await?;
        }
        drop(connection);
        let payload = serde_json::to_string(&crate::wire::WireTrack::from(track))?;
        self.store_payload(
            item,
            instance,
            Operation::Catalog,
            Status::Found,
            Some(&payload),
        )
        .await
    }

    /// Stores a catalog album's record and every row it names.
    async fn store_catalog_album(
        &self,
        item: &QueueItem,
        instance: Uuid,
        slug: &Slug,
        album: &hearthsound_core::providers::CatalogAlbum,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        self.resolver
            .album(&mut connection, instance, slug, album)
            .await?;
        drop(connection);
        let payload = serde_json::to_string(&crate::wire::WireAlbum::from(album))?;
        self.store_payload(
            item,
            instance,
            Operation::Catalog,
            Status::Found,
            Some(&payload),
        )
        .await
    }

    /// Stores a catalog artist's record.
    async fn store_catalog_artist(
        &self,
        item: &QueueItem,
        instance: Uuid,
        slug: &Slug,
        artist: &hearthsound_core::providers::CatalogArtist,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        self.resolver
            .artist(&mut connection, instance, slug, artist)
            .await?;
        drop(connection);
        let payload = serde_json::to_string(&crate::wire::WireArtist::from(artist))?;
        self.store_payload(
            item,
            instance,
            Operation::Catalog,
            Status::Found,
            Some(&payload),
        )
        .await
    }

    /// Stores a provider's answer that carries no payload.
    async fn store_answer(
        &self,
        item: &QueueItem,
        instance: Uuid,
        operation: Operation,
        status: Status,
        payload: Option<&str>,
    ) -> Result<(), CatalogError> {
        self.store_payload(item, instance, operation, status, payload)
            .await
    }

    /// Stores one answer on its own connection.
    async fn store_payload(
        &self,
        item: &QueueItem,
        instance: Uuid,
        operation: Operation,
        status: Status,
        payload: Option<&str>,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        sources::put(
            &mut connection,
            item.kind,
            item.entity,
            instance,
            operation,
            status,
            payload,
        )
        .await
    }

    /// Reads an entity's stored identifiers.
    async fn ids_of(
        &self,
        kind: EntityKind,
        entity: Uuid,
    ) -> Result<Vec<ExternalId>, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        external_ids::ids_of(&mut connection, kind, entity)
            .await
            .map_err(CatalogError::Library)
    }

    /// Reads an entity's title: an album's title or an artist's name.
    async fn title_of(&self, kind: EntityKind, entity: Uuid) -> Result<String, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let title: Option<String> = match kind {
            EntityKind::Album => {
                sqlx::query_scalar("SELECT title FROM albums WHERE id = ?")
                    .bind(entity.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?
            }
            EntityKind::Artist => {
                sqlx::query_scalar("SELECT name FROM artists WHERE id = ?")
                    .bind(entity.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?
            }
            _ => None,
        };
        Ok(title.unwrap_or_default())
    }

    /// Reads an artist's name.
    async fn artist_name(&self, entity: Uuid) -> Result<String, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM artists WHERE id = ?")
            .bind(entity.as_bytes().as_slice())
            .fetch_optional(&mut *connection)
            .await?;
        Ok(name.unwrap_or_default())
    }
}

/// Returns whether a release belongs in a discography.
///
/// # Parameters
///
/// - `album_type`: the release's type as the provider reported it, lowercase,
///   or `None` when it reported none.
///
/// # Returns
///
/// `true` for an album, an EP and a single, and for a release whose type the
/// provider did not state.
pub(crate) fn is_discography_release(album_type: Option<&str>) -> bool {
    match album_type {
        None => true,
        Some(kind) => matches!(
            kind.to_ascii_lowercase().as_str(),
            "album" | "ep" | "single"
        ),
    }
}

/// Returns whether a provider answer means "ask again in a moment".
///
/// The entity's page is left as it is and the queue row is deferred, so a
/// service that throttles us does not leave a page marked fetched with nothing
/// stored on it.
///
/// # Parameters
///
/// - `error`: the provider's failure.
fn throttled(error: &ProviderError) -> bool {
    matches!(error, ProviderError::RateLimited { .. })
}

/// Calls a provider, waiting out a rate limit and trying again.
///
/// A provider that answers `RateLimited` is asked again after the delay it named,
/// at least [`MIN_THROTTLE_WAIT_MS`], up to [`THROTTLE_RETRIES`] times. A
/// one-request-per-second service is then answered within a pass rather than
/// abandoning the pass and restarting the whole walk from the queue, which spent
/// the same budget on the requests it had already made.
///
/// # Parameters
///
/// - `call`: the provider call, called once per attempt.
///
/// # Returns
///
/// The call's answer, or its last error once the retries are spent.
async fn retry_throttled<T, F, Fut>(mut call: F) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    let mut attempt = 0_u32;
    loop {
        match call().await {
            Err(ProviderError::RateLimited { retry_after_ms }) if attempt < THROTTLE_RETRIES => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(
                    retry_after_ms.max(MIN_THROTTLE_WAIT_MS),
                ))
                .await;
            }
            other => return other,
        }
    }
}

/// The priority remote cards are prefetched at.
///
/// Below every held item: a library change is `1` and a viewer's request is `0`,
/// so the covers of remote albums and similar artists are fetched only when no
/// held work waits.
const PREFETCH_PRIORITY: i64 = 2;

/// How many times a throttled provider call is retried inside one pass.
const THROTTLE_RETRIES: u32 = 3;

/// The shortest wait before a throttled call is retried.
///
/// A provider that answers `rate limited; retry after 0 ms` - a burst that is
/// already spent - would otherwise be hammered.
const MIN_THROTTLE_WAIT_MS: u64 = 1_000;

/// Returns the provider id an instance issued for an entity.
///
/// # Parameters
///
/// - `ids`: the entity's stored identifiers.
/// - `slug`: the instance.
///
/// # Returns
///
/// The provider's own id, or `None` when the entity carries none for it.
fn provider_id(ids: &[ExternalId], slug: &Slug) -> Option<String> {
    let namespace = provider_namespace(slug);
    ids.iter()
        .find(|id| id.namespace == namespace)
        .map(|id| id.value.clone())
}

/// Returns the MusicBrainz artist id an entity's identifiers hold.
///
/// # Parameters
///
/// - `ids`: the entity's stored identifiers.
///
/// # Returns
///
/// The `mb-artist` value, or `None` when the entity carries none.
fn mb_artist_of(ids: &[ExternalId]) -> Option<&str> {
    ids.iter()
        .find(|id| id.namespace == MB_ARTIST)
        .map(|id| id.value.as_str())
}

/// Returns the current time in unix milliseconds.
pub(crate) fn unix_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// Reads the view deadline, the refresh window and the precedence setting.
///
/// # Parameters
///
/// - `settings`: the current snapshot.
///
/// # Returns
///
/// The deadline for an inline walk, the age at which stored data is stale, and
/// the precedence.
pub fn view_policy(
    settings: &hearthsound_config::SettingsStore,
) -> (Duration, Duration, Precedence) {
    let timeout = settings
        .get(keys::INDEX_VIEW_TIMEOUT_MS)
        .unwrap_or(5000)
        .clamp(500, 30_000) as u64;
    let refresh = settings
        .get(keys::INDEX_REFRESH_DAYS)
        .unwrap_or(30)
        .clamp(1, 3650) as u64;
    let precedence = Precedence::parse(
        &settings
            .get(keys::METADATA_PRECEDENCE)
            .unwrap_or_else(|_| "tags".to_owned()),
    );
    (
        Duration::from_millis(timeout),
        Duration::from_secs(refresh * 86_400),
        precedence,
    )
}

/// Publishes `entity.updated` for a stored change.
///
/// # Parameters
///
/// - `events`: the bus.
/// - `kind`: the entity kind.
/// - `id`: the row's raw primary key.
pub(crate) fn publish_updated(events: &EventBus, kind: EntityKind, id: Uuid) {
    let prefix = match kind {
        EntityKind::Track => hearthsound_core::IdPrefix::Track,
        EntityKind::Album => hearthsound_core::IdPrefix::Album,
        EntityKind::Artist => hearthsound_core::IdPrefix::Artist,
        EntityKind::Playlist => hearthsound_core::IdPrefix::Playlist,
    };
    events.publish_for(
        Audience::Everyone,
        ServerEvent::EntityUpdated(EntityUpdated {
            id: hearthsound_core::EntityId::from_uuid(prefix, id).to_string(),
            kind,
        }),
    );
}
