//! Serving the picture of an entity, from the server's own store.
//!
//! Every image the server shows is stored: a scan stores the cover it read from
//! a file, and the indexer stores what a provider named. Serving is therefore a
//! read and, for a rendition other than the original, a thumbnail
//! ([`hearthsound_library::thumbnails::ThumbnailCache`]) - never a provider call
//! on the request path.
//!
//! One image is chosen per request: the file's own image when the precedence
//! puts tags first, then the artwork layer's instances in the administrator's
//! order, then the catalog layer's, and finally the first stored rendition.
//! Albums and album-less tracks serve a `cover`, artists a `portrait`; a track
//! with an album serves the album's cover, and an artist without a portrait
//! serves the cover of its newest album that has one.
//!
//! An image row whose bytes are not stored yet but whose provider URL is known
//! is fetched through the instance that named it, stored, and then served. A
//! provider that refuses the URL (403, 404, 410) costs that row its place, so the
//! next candidate is tried and the refusal is not repeated forever.
//!
//! Indexing stores the bytes of the image it names, so a stored page draws its
//! pictures with no network at all. An entity with no stored image is queued for
//! the indexer and answered with nothing - the client re-reads when the indexer
//! lands - and nothing on this path ever waits for a provider.

use hearthsound_core::{EntityKind, Slug};
use hearthsound_db::Database;
use hearthsound_library::images::{self, ImageStore, StoredImage};
use hearthsound_library::thumbnails::{ThumbnailCache, thumbnail_size};
use hearthsound_plugin_host::http::HttpError;
use hearthsound_plugin_host::{FetchError, ProviderRegistry};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::CatalogError;

/// One image, ready to be written to a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedImage {
    /// The encoded bytes, original or scaled.
    pub bytes: Vec<u8>,
    /// The media type of those bytes.
    pub content_type: String,
    /// The strong validator a client revalidates with.
    pub etag: String,
}

/// The images the server keeps for entities, and how it answers a request.
pub struct ImageService {
    store: ImageStore,
    thumbs: std::sync::Arc<ThumbnailCache>,
    registry: std::sync::Arc<ProviderRegistry>,
    database: std::sync::Arc<Database>,
}

impl ImageService {
    /// Builds the service over the artwork directory.
    ///
    /// # Parameters
    ///
    /// - `registry`: the provider registry, used to fetch an image the server
    ///   has not stored yet.
    /// - `database`: the open database.
    /// - `artwork_root`: the directory blobs are written under.
    /// - `ffmpeg`: the runner the thumbnail cache scales with.
    pub fn new(
        registry: std::sync::Arc<ProviderRegistry>,
        database: std::sync::Arc<Database>,
        artwork_root: impl Into<std::path::PathBuf>,
        ffmpeg: std::sync::Arc<hearthsound_media::FfmpegRunner>,
    ) -> ImageService {
        let root = artwork_root.into();
        ImageService {
            store: ImageStore::new(root.clone()),
            thumbs: std::sync::Arc::new(ThumbnailCache::new(root.join("thumbs"), ffmpeg)),
            registry,
            database,
        }
    }

    /// Returns the store the service writes to.
    pub fn store(&self) -> &ImageStore {
        &self.store
    }

    /// Chooses the stored image of an entity, fetching its bytes if needed.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the row's raw primary key.
    ///
    /// # Returns
    ///
    /// The chosen image with its bytes, or `None` when the server has neither an
    /// image nor a way to get one.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a database read fails.
    pub async fn primary(
        &self,
        kind: EntityKind,
        id: Uuid,
    ) -> Result<Option<ServedImage>, CatalogError> {
        let Some((entity, role)) = self.subject(kind, id).await? else {
            return Ok(None);
        };
        let mut connection = self.database.readers().acquire().await?;
        let candidates = images::images_of(&mut connection, entity.0, entity.1, role)
            .await
            .map_err(CatalogError::Library)?;
        drop(connection);
        // A stored rendition is served first, whatever order the instances named
        // the candidates in: the bytes are what the page draws, and a candidate
        // whose download keeps failing would otherwise cost its fetch on every
        // read of a page that already has a picture.
        let mut unstated = Vec::new();
        for candidate in candidates {
            match self.stored(&candidate).await? {
                Some(served) => return Ok(Some(served)),
                None => unstated.push(candidate),
            }
        }
        for candidate in unstated {
            if let Some(served) = self.bytes_of(&candidate).await? {
                return Ok(Some(served));
            }
        }
        Ok(None)
    }

    /// Serves an entity's image, scaled when a size is asked for.
    ///
    /// A stored image is served as it is. When nothing is stored, the entity's
    /// head walk is queued and the request is answered with nothing, so the
    /// client re-reads when `entity.updated` arrives. A cover is never worth
    /// making a reader wait for a provider: the queue front is where it belongs,
    /// and the picture appears a second later.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the row's raw primary key.
    /// - `size`: the requested size, or `None` for the original bytes.
    ///
    /// # Returns
    ///
    /// The image, or `None` when the entity has none yet.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read, a fetch or a scale fails.
    pub async fn serve(
        &self,
        kind: EntityKind,
        id: Uuid,
        size: Option<u32>,
    ) -> Result<Option<ServedImage>, CatalogError> {
        if let Some(original) = self.primary(kind, id).await? {
            return self.scale(original, size).await;
        }
        if self.head_missing(kind, id).await? {
            self.queue_image(kind, id).await?;
        }
        Ok(None)
    }

    /// Reports whether an entity's head walk has never been stored.
    ///
    /// Only an entity whose head was never walked is queued for a picture: a
    /// walk that already ran asked the artwork layer and found nothing, so
    /// queueing it again on every read of a cover-less album would walk it once
    /// per read - and every walk publishes `entity.updated`, which is what makes
    /// the page read again. An entity that gains an artwork instance later is
    /// picked up by the refresh sweep.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind, before a track is mapped to its album.
    /// - `id`: the row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    async fn head_missing(&self, kind: EntityKind, id: Uuid) -> Result<bool, CatalogError> {
        let Some((entity, _)) = self.subject(kind, id).await? else {
            return Ok(false);
        };
        let mut connection = self.database.readers().acquire().await?;
        let fetched: Option<Option<i64>> = match entity.0 {
            EntityKind::Album => {
                sqlx::query_scalar("SELECT fetched_at FROM albums WHERE id = ?")
                    .bind(entity.1.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?
            }
            EntityKind::Artist => {
                sqlx::query_scalar("SELECT fetched_at FROM artists WHERE id = ?")
                    .bind(entity.1.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?
            }
            _ => {
                sqlx::query_scalar("SELECT fetched_at FROM tracks WHERE id = ?")
                    .bind(entity.1.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?
            }
        };
        Ok(fetched.flatten().is_none())
    }

    /// Queues the head walk whose images an entity's picture comes from.
    ///
    /// A track's picture is its album's cover, so the album's head is what the
    /// indexer has to store.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read or a write fails.
    async fn queue_image(&self, kind: EntityKind, id: Uuid) -> Result<(), CatalogError> {
        let (kind, id) = match kind {
            EntityKind::Track => {
                let mut connection = self.database.readers().acquire().await?;
                let album: Option<Option<Vec<u8>>> =
                    sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?")
                        .bind(id.as_bytes().as_slice())
                        .fetch_optional(&mut *connection)
                        .await?;
                drop(connection);
                match album.flatten() {
                    Some(bytes) if bytes.len() == 16 => (
                        EntityKind::Album,
                        Uuid::from_slice(&bytes).map_err(|_| CatalogError::CorruptId)?,
                    ),
                    _ => (EntityKind::Track, id),
                }
            }
            other => (other, id),
        };
        let mut connection = self.database.writer().acquire().await?;
        hearthsound_library::index_queue::enqueue(
            &mut connection,
            kind,
            id,
            hearthsound_library::index_queue::Depth::Head,
            hearthsound_library::index_queue::PRIORITY_VIEW,
        )
        .await
        .map_err(CatalogError::Library)?;
        Ok(())
    }

    /// Reads the image of a playlist: the first entry that has one.
    ///
    /// # Parameters
    ///
    /// - `entries`: the playlist's track ids, in order.
    ///
    /// # Returns
    ///
    /// The first servable image among the entries.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`ImageService::primary`].
    pub async fn playlist(&self, entries: &[Uuid]) -> Result<Option<ServedImage>, CatalogError> {
        for entry in entries {
            if let Some(image) = self.primary(EntityKind::Track, *entry).await? {
                return Ok(Some(image));
            }
        }
        Ok(None)
    }

    /// Returns the entity and role whose image an id is served from.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the row's raw primary key.
    async fn subject(
        &self,
        kind: EntityKind,
        id: Uuid,
    ) -> Result<Option<((EntityKind, Uuid), &'static str)>, CatalogError> {
        match kind {
            EntityKind::Album => Ok(Some(((EntityKind::Album, id), "cover"))),
            EntityKind::Artist => Ok(Some(((EntityKind::Artist, id), "portrait"))),
            EntityKind::Track => {
                let mut connection = self.database.readers().acquire().await?;
                let album: Option<Option<Vec<u8>>> =
                    sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?")
                        .bind(id.as_bytes().as_slice())
                        .fetch_optional(&mut *connection)
                        .await?;
                drop(connection);
                match album.flatten() {
                    Some(bytes) if bytes.len() == 16 => {
                        let album =
                            Uuid::from_slice(&bytes).map_err(|_| CatalogError::CorruptId)?;
                        Ok(Some(((EntityKind::Album, album), "cover")))
                    }
                    _ => Ok(Some(((EntityKind::Track, id), "cover"))),
                }
            }
            EntityKind::Playlist => Ok(None),
        }
    }

    /// Reads the bytes one image row already stores.
    ///
    /// # Parameters
    ///
    /// - `image`: the image row.
    ///
    /// # Returns
    ///
    /// The image, or `None` when its bytes are not stored yet.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the store cannot be read.
    async fn stored(&self, image: &StoredImage) -> Result<Option<ServedImage>, CatalogError> {
        let Some(path) = &image.blob_path else {
            return Ok(None);
        };
        let Some(bytes) = self
            .store
            .read_blob(path)
            .await
            .map_err(CatalogError::Library)?
        else {
            return Ok(None);
        };
        let content_type = image
            .content_type
            .clone()
            .unwrap_or_else(|| "image/jpeg".to_owned());
        Ok(Some(ServedImage {
            etag: etag(&bytes),
            bytes,
            content_type,
        }))
    }

    /// Reads one image's bytes, fetching and storing them when they are absent.
    async fn bytes_of(&self, image: &StoredImage) -> Result<Option<ServedImage>, CatalogError> {
        if let Some(served) = self.stored(image).await? {
            return Ok(Some(served));
        }
        let (Some(url), Some(instance)) = (&image.url, image.instance_id) else {
            tracing::debug!(id = %image.id, "image row lacks a url or instance");
            return Ok(None);
        };
        let slug = self.slug_of(instance);
        let Some(slug) = slug else {
            tracing::debug!(url, "no instance for an image");
            return Ok(None);
        };
        let fetched = match self.registry.fetch_image(slug.as_str(), url).await {
            Ok(fetched) => fetched,
            Err(FetchError::Http(HttpError::Denied(reason))) => {
                tracing::debug!(url, %reason, "the server may not fetch an image it found");
                return Ok(None);
            }
            Err(error) => {
                tracing::debug!(url, %error, "could not fetch an image");
                return Ok(None);
            }
        };
        match fetched.status {
            200..=299 => {}
            403 | 404 | 410 => {
                let mut connection = self.database.writer().acquire().await?;
                images::delete_image(&mut connection, image.id)
                    .await
                    .map_err(CatalogError::Library)?;
                return Ok(None);
            }
            _ => return Ok(None),
        }
        let content_type = fetched
            .content_type
            .clone()
            .unwrap_or_else(|| "image/jpeg".to_owned());
        let mut connection = self.database.writer().acquire().await?;
        let hash = self
            .store
            .store(&mut connection, &fetched.bytes, &content_type)
            .await
            .map_err(CatalogError::Library)?;
        images::attach_blob(&mut connection, image.id, &hash)
            .await
            .map_err(CatalogError::Library)?;
        drop(connection);
        Ok(Some(ServedImage {
            etag: etag(&fetched.bytes),
            bytes: fetched.bytes,
            content_type,
        }))
    }

    /// Scales a served image to the requested size.
    async fn scale(
        &self,
        image: ServedImage,
        size: Option<u32>,
    ) -> Result<Option<ServedImage>, CatalogError> {
        let Some(size) = size else {
            return Ok(Some(image));
        };
        let Some(target) = thumbnail_size(Some(size)) else {
            return Ok(Some(image));
        };
        let bytes = self
            .thumbs
            .scaled(&image.bytes, target)
            .await
            .map_err(CatalogError::Library)?;
        Ok(Some(ServedImage {
            etag: etag(&bytes),
            bytes,
            content_type: "image/jpeg".to_owned(),
        }))
    }

    /// Returns the slug of an instance by its id.
    fn slug_of(&self, instance: Uuid) -> Option<Slug> {
        self.registry
            .instances()
            .into_iter()
            .find(|known| known.id() == instance)
            .map(|known| known.slug().clone())
    }
}

/// Returns the strong validator of some bytes.
///
/// The first 16 bytes of the SHA-256, hex encoded: the bytes are what a client
/// caches, so their hash is the validator.
///
/// # Parameters
///
/// - `bytes`: the image bytes.
fn etag(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    data_encoding::HEXLOWER.encode(&digest[..16])
}
