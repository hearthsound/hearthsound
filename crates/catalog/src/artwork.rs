//! Cover art for catalog entities, fetched through the instance that issued
//! them.
//!
//! A client asks `getCoverArt` for an id that names a catalog entity, and the
//! server has to produce bytes. Nothing in the catalog layer holds an image:
//! what a provider returns is a list of URLs with the dimensions it describes
//! them with, so this module picks the best of them for the requested size,
//! fetches it through the issuing instance's HTTP grant, and returns it.
//!
//! The fetch is the host's, not the plugin's: it goes through the same client,
//! and therefore the same allowlist, address policy, redirect re-checks, timeout
//! and body cap, that the plugin itself gets from `host-http`. A URL the
//! instance could not reach itself is denied here too.
//!
//! Whether the bytes may be kept is the instance's decision, declared in its
//! manifest as `catalog.imageCachePolicy` (03-plugin-system.md, section 9.3):
//!
//! - `cache` (the default) stores the image under `<data>/artwork/` and records
//!   it in `artwork_cache`, so the next request for the same image is served
//!   from disk;
//! - `no-store` returns the image without writing anything, which is what a
//!   provider whose terms forbid caching requires.
//!
//! The cache is an optimization, never a correctness requirement: a row whose
//! file is gone, or a file that cannot be read or written, is a miss with a log
//! line, and the request is answered from the provider instead.

use std::path::Path;
use std::time::Duration;

use hearthsound_core::providers::{ArtworkSubject, ExternalId, Image};
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix, Layer, Slug};
use hearthsound_library::LibraryRepository;
use hearthsound_library::artwork::mime_for;
use hearthsound_plugin_host::FetchError;
use hearthsound_plugin_host::http::HttpError;
use hearthsound_plugin_host::manifest::ImageCachePolicy;
use hearthsound_plugin_host::registry::RegistryInstance;
use uuid::Uuid;

use crate::cache::RequestKey;
use crate::error::CatalogError;
use crate::materialize::{MB_ARTIST, MB_RELEASE, MB_RELEASE_GROUP, ProviderAlias, UPC};
use crate::service::CatalogService;
use crate::wire::{WireImage, WireImages};

/// How long an artwork answer that found images stays fresh.
const ARTWORK_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long an artwork answer that found none stays fresh.
///
/// An empty answer is cached for less time, because an instance that has no
/// picture of an album today may have one tomorrow.
const EMPTY_ARTWORK_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long an image URL whose host refused it is not asked again.
///
/// Ten minutes. An image host that answers a burst of requests with a refusal,
/// throttling being the usual reason, keeps refusing while the burst lasts; the
/// window outlasts one page of covers and is short enough that a temporary
/// refusal does not hide a picture for the rest of the day.
pub(crate) const REFUSED_IMAGE_TTL: Duration = Duration::from_secs(10 * 60);

/// What a caller asks for cover art of.
///
/// The id is either a catalog id (`x_<slug>_<kind>_<payload>`, the entity is
/// still only in the provider's catalog) or a stored entity id whose
/// `p:<slug>` alias names the instance that issued it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverArtRequest {
    /// The entity to serve cover art of.
    pub id: ClientId,
    /// The requested size in pixels, when the caller named one.
    ///
    /// The size is a maximum: the smallest image the provider describes as at
    /// least that large is served, and when the provider has none that large the
    /// largest it has is served instead.
    pub size: Option<u32>,
}

/// One cover image, ready to be written to a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverArt {
    /// The image's MIME type.
    pub mime_type: String,
    /// The encoded image bytes.
    pub bytes: Vec<u8>,
}

/// What the artwork layer answered for one request.
enum LayerCover {
    /// An instance delivered the picture.
    Found(CoverArt),
    /// No instance delivered a picture.
    Missing {
        /// The last instance that failed in a way a retry may get past, when
        /// one did.
        transient: Option<String>,
    },
}

impl CatalogService {
    /// Serves the cover art of an entity.
    ///
    /// The artwork layer is asked first: every instance in the `artwork` layer,
    /// in the order the administrator set, is asked for the entity's picture
    /// under `artwork.timeout_ms`, and the first one that delivers an image has
    /// it served. An instance that fails is skipped, and an entity no artwork
    /// instance delivers a picture of falls back to the instance that issued
    /// it, which is where its own images are.
    ///
    /// # Parameters
    ///
    /// - `request`: the entity and the requested size.
    ///
    /// # Returns
    ///
    /// The image's MIME type and its bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when no instance knows an image of the
    /// entity and when the image's URL answers with a not-found, and
    /// [`CatalogError::Unavailable`] when nothing answered and an instance could
    /// not answer right now, which is what a client retries.
    pub async fn cover_art(&self, request: &CoverArtRequest) -> Result<CoverArt, CatalogError> {
        let transient = match self.artwork_cover(request).await? {
            LayerCover::Found(cover) => return Ok(cover),
            LayerCover::Missing { transient } => transient,
        };
        match self.issuing_cover_art(request).await {
            Err(CatalogError::NotFound) => match transient {
                Some(instance) => Err(CatalogError::Unavailable { instance }),
                None => Err(CatalogError::NotFound),
            },
            answered => answered,
        }
    }

    /// Returns an artist's portrait renditions from the artwork layer.
    ///
    /// The first artwork instance, in the administrator's order, that answers a
    /// non-empty image list wins. Nothing is fetched: the URLs are what a client
    /// receives, and an instance that describes them has already promised they
    /// are reachable. An instance that fails is logged and skipped, and a
    /// stored artist's local album art is not substituted, because a caller
    /// that receives an empty list falls back to its own picture.
    ///
    /// # Parameters
    ///
    /// - `id`: a stored artist id, or the catalog id of an artist.
    ///
    /// # Returns
    ///
    /// The portrait images the layer describes, in the answering instance's
    /// order, with their dimensions where the provider states them.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id names no artist, and
    /// [`CatalogError::Database`] when the artist cannot be read. A provider
    /// failure is logged and skipped, never returned.
    pub async fn artist_portraits(&self, id: &ClientId) -> Result<Vec<Image>, CatalogError> {
        let subject = self.subject(id).await?;
        if subject.kind != EntityKind::Artist {
            return Err(CatalogError::NotFound);
        }
        let instances = self
            .registry()
            .layers(Layer::Artwork)
            .into_iter()
            .filter(|instance| instance.artwork().is_some())
            .collect::<Vec<_>>();
        if instances.is_empty() {
            return Ok(Vec::new());
        }
        let timeout = artwork_timeout(&self.settings_snapshot())?;
        let key = RequestKey::artwork(subject.kind, &subject);
        let deadline = tokio::time::Instant::now() + timeout;

        // Every instance is asked at once and the first answer in the
        // administrator's order wins, so a portrait costs one instance's deadline
        // rather than the sum of them.
        let answers =
            futures_util::future::join_all(instances.iter().map(|instance| {
                self.layer_images(instance, &subject, &key, deadline, "a portrait")
            }))
            .await;
        for images in answers.into_iter().flatten() {
            if !images.is_empty() {
                return Ok(images);
            }
        }
        Ok(Vec::new())
    }

    /// Returns the images one artwork instance described, or `None` when it did
    /// not answer before the deadline.
    ///
    /// A cached answer is returned without asking the instance, and an answer that
    /// was fetched is stored under the lifetime its emptiness deserves.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance to ask.
    /// - `subject`: the subject the picture is for.
    /// - `key`: the subject's cache key.
    /// - `deadline`: the instant the whole fan-out stops waiting.
    /// - `operation`: what was asked for, for the log lines.
    ///
    /// # Returns
    ///
    /// The images, or `None` when the instance failed or was too slow.
    async fn layer_images(
        &self,
        instance: &RegistryInstance,
        subject: &ArtworkSubject,
        key: &RequestKey,
        deadline: tokio::time::Instant,
        operation: &'static str,
    ) -> Option<Vec<Image>> {
        let provider = instance.artwork()?;
        if let Some(images) = self.cached_images(instance.id(), key).await {
            return Some(images);
        }
        match tokio::time::timeout_at(deadline, provider.find_artwork(subject)).await {
            Ok(Ok(images)) => {
                let ttl = if images.is_empty() {
                    EMPTY_ARTWORK_TTL
                } else {
                    ARTWORK_TTL
                };
                self.store_images(instance.id(), key, &images, ttl).await;
                Some(images)
            }
            Ok(Err(error)) => {
                tracing::debug!(
                    instance = instance.slug().as_str(),
                    %error,
                    operation,
                    "an artwork instance could not answer"
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    instance = instance.slug().as_str(),
                    operation,
                    "an artwork instance did not answer in time"
                );
                None
            }
        }
    }

    /// Reads the portrait renditions the artwork layer already described.
    ///
    /// No instance is asked: the subject is built from the stored row, or from the
    /// cached artist for a catalog id, and the first cached answer in the
    /// administrator's order wins.
    ///
    /// # Parameters
    ///
    /// - `id`: a stored artist id, or the catalog id of an artist.
    ///
    /// # Returns
    ///
    /// The cached renditions, or `None` when no instance answered for the artist
    /// yet.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id names no artist and
    /// [`CatalogError::Database`] when the stored artist cannot be read.
    pub async fn cached_artist_portraits(
        &self,
        id: &ClientId,
    ) -> Result<Option<Vec<Image>>, CatalogError> {
        let subject = match id {
            ClientId::Entity(entity) => Some(self.stored_subject(*entity).await?),
            ClientId::Catalog(catalog) => {
                self.cached_artist(catalog)
                    .await?
                    .map(|found| ArtworkSubject {
                        kind: EntityKind::Artist,
                        provider_id: Some(catalog.provider_id().to_owned()),
                        ids: found.artist.ids,
                        title: found.artist.name,
                        artists: Vec::new(),
                    })
            }
        };
        let Some(subject) = subject else {
            return Ok(None);
        };
        if subject.kind != EntityKind::Artist {
            return Err(CatalogError::NotFound);
        }
        let key = RequestKey::artwork(subject.kind, &subject);
        for instance in self.registry().layers(Layer::Artwork) {
            if let Some(images) = self.cache().read::<WireImages>(instance.id(), &key).await {
                return Ok(Some(images.0.into_iter().map(Image::from).collect()));
            }
        }
        Ok(None)
    }

    /// Asks the artwork layer for an entity's picture.
    ///
    /// Every instance is asked in layer order until one delivers an image. An
    /// instance that fails, whether it cannot answer the lookup or cannot
    /// deliver the image it described, is logged and skipped, so one slow or
    /// broken image host never hides the pictures the next instance knows.
    ///
    /// # Parameters
    ///
    /// - `request`: the entity and the requested size.
    ///
    /// # Returns
    ///
    /// The image when an instance delivers one; otherwise
    /// [`LayerCover::Missing`] naming the last instance that failed in a way a
    /// retry may get past.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the subject of the request cannot be
    /// described or the artwork deadline cannot be read.
    async fn artwork_cover(&self, request: &CoverArtRequest) -> Result<LayerCover, CatalogError> {
        let instances = self
            .registry()
            .layers(Layer::Artwork)
            .into_iter()
            .filter(|instance| instance.artwork().is_some())
            .collect::<Vec<_>>();
        if instances.is_empty() {
            return Ok(LayerCover::Missing { transient: None });
        }
        let subject = self.subject(&request.id).await?;
        let timeout = artwork_timeout(&self.settings_snapshot())?;
        let key = RequestKey::artwork(subject.kind, &subject);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut transient = None;

        // The lookup is a provider call per instance, so every instance is asked
        // at once; the bytes are fetched from the first instance in the
        // administrator's order that described an image, which keeps the transfer
        // on one instance.
        let answers = futures_util::future::join_all(
            instances
                .iter()
                .map(|instance| self.layer_images(instance, &subject, &key, deadline, "a cover")),
        )
        .await;
        for (instance, answer) in instances.iter().zip(answers) {
            let Some(images) = answer else {
                // The failure was logged where it happened; a retry may get past
                // an unavailable instance, a missing image may not.
                transient = Some(instance.slug().as_str().to_owned());
                continue;
            };
            let Some(image) = pick_image(&images, request.size) else {
                continue;
            };
            match self
                .fetch_layer_image(instance, subject.kind, &subject, image)
                .await
            {
                Ok(cover) => return Ok(LayerCover::Found(cover)),
                Err(
                    CatalogError::Unavailable { instance }
                    | CatalogError::RateLimited { instance, .. },
                ) => {
                    transient = Some(instance);
                }
                // A not-found image and a host the instance may not reach were
                // logged where they happened; the next instance may still know
                // a picture.
                Err(_) => {}
            }
        }

        Ok(LayerCover::Missing { transient })
    }

    /// Serves the cover art of a catalog entity through its issuing instance.
    ///
    /// The image is picked from the ones the entity's provider describes for the
    /// requested size, fetched under the issuing instance's HTTP permissions, and
    /// stored when that instance's manifest allows it. A stored image is served
    /// from disk afterwards without asking the provider again.
    ///
    /// # Parameters
    ///
    /// - `request`: the entity and the requested size.
    ///
    /// # Returns
    ///
    /// The image's MIME type and its bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id names no catalog entity,
    /// when its issuing instance is not served, when the entity describes no
    /// image, and when the image's URL answers with a not-found.
    /// [`CatalogError::Unavailable`] is returned when the instance cannot deliver
    /// the image right now, and [`CatalogError::Internal`] when the instance is
    /// not allowed to reach the image's host at all, which is a configuration
    /// problem rather than a transient one.
    async fn issuing_cover_art(&self, request: &CoverArtRequest) -> Result<CoverArt, CatalogError> {
        let (slug, kind, provider_id) = self.issuing_instance(&request.id).await?;
        let images = self.entity_images(&slug, kind, &provider_id).await?;
        let image = pick_image(&images, request.size).ok_or(CatalogError::NotFound)?;
        let key = cache_key(&slug, kind, &provider_id, image);

        if let Some(cached) = self.cached_cover(&key).await {
            return Ok(cached);
        }

        if self.image_missing(&slug, &image.url).await {
            tracing::debug!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                "an image host has no image at this URL; not asking again yet"
            );
            return Err(CatalogError::NotFound);
        }

        if self.image_refused(&slug, &image.url).await {
            tracing::debug!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                "an image host refused this URL recently; not asking again yet"
            );
            return Err(CatalogError::Unavailable {
                instance: slug.as_str().to_owned(),
            });
        }

        let fetched = self
            .registry()
            .fetch_image(slug.as_str(), &image.url)
            .await
            .map_err(|error| fetch_failure(&slug, &image.url, error))?;
        if is_missing_image(fetched.status) {
            tracing::debug!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                status = fetched.status,
                "the catalog instance has no image at that URL"
            );
            self.mark_image_missing(&slug, &image.url).await;
            return Err(CatalogError::NotFound);
        }
        if !(200..300).contains(&fetched.status) {
            tracing::warn!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                status = fetched.status,
                "the catalog instance answered an image request with a failure"
            );
            self.refuse_image(&slug, &image.url).await;
            return Err(CatalogError::Unavailable {
                instance: slug.as_str().to_owned(),
            });
        }

        let mime_type = fetched
            .content_type
            .clone()
            .unwrap_or_else(|| mime_for(image_url_name(&image.url)));
        if fetched.cache_policy == ImageCachePolicy::Cache {
            self.store_cover(&key, &slug, image, &mime_type, &fetched.bytes)
                .await;
        }

        Ok(CoverArt {
            mime_type,
            bytes: fetched.bytes,
        })
    }

    /// Returns the instance, entity kind and provider id an id resolves to.
    ///
    /// A catalog id names all three by itself. A stored entity id names them
    /// through its `p:<slug>` alias, which is what materialization recorded when
    /// the entity was stored; an entity with several such aliases is served by
    /// the first instance in slug order, so the same request always reports the
    /// same instance.
    ///
    /// # Parameters
    ///
    /// - `id`: the id the caller asked cover art of.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id names no catalog entity and
    /// when no provider alias points at it.
    async fn issuing_instance(
        &self,
        id: &ClientId,
    ) -> Result<(Slug, EntityKind, String), CatalogError> {
        match id {
            ClientId::Catalog(catalog) => Ok((
                catalog.slug().clone(),
                catalog.kind(),
                catalog.provider_id().to_owned(),
            )),
            ClientId::Entity(entity) => {
                let alias = self.first_provider_alias(*entity).await?.ok_or_else(|| {
                    tracing::debug!(
                        entity = %entity,
                        "no catalog instance issued this entity"
                    );
                    CatalogError::NotFound
                })?;
                let kind = entity.kind().entity_kind().ok_or(CatalogError::NotFound)?;
                Ok((alias.slug, kind, alias.provider_id))
            }
        }
    }

    /// Returns the first provider alias of a stored entity, in slug order.
    ///
    /// # Parameters
    ///
    /// - `entity`: the stored entity.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the query fails and
    /// [`CatalogError::CorruptAlias`] when a stored id does not decode.
    async fn first_provider_alias(
        &self,
        entity: EntityId,
    ) -> Result<Option<ProviderAlias>, CatalogError> {
        Ok(self.provider_aliases(entity).await?.into_iter().next())
    }

    /// Returns the images an entity's provider describes.
    ///
    /// A track has no images of its own in the catalog interface; the images a
    /// client expects for it are its album's, which is what every catalog
    /// provider documents and what this returns.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance that issued the entity.
    /// - `kind`: the entity kind.
    /// - `provider_id`: the provider's own id for the entity.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the kind has no images at all and
    /// when the provider does not know the entity, and the mapped provider
    /// failure when the lookup fails.
    async fn entity_images(
        &self,
        slug: &Slug,
        kind: EntityKind,
        provider_id: &str,
    ) -> Result<Vec<Image>, CatalogError> {
        let id = CatalogId::new(slug.clone(), kind, provider_id).map_err(|_| {
            CatalogError::InvalidEntity {
                instance: slug.as_str().to_owned(),
            }
        })?;
        match kind {
            EntityKind::Track => Ok(self
                .track(&id)
                .await?
                .track
                .album
                .map(|album| album.images)
                .unwrap_or_default()),
            EntityKind::Album => Ok(self.album(&id).await?.album.images),
            EntityKind::Artist => Ok(self.artist(&id).await?.artist.images),
            EntityKind::Playlist => Err(CatalogError::NotFound),
        }
    }

    /// Builds the subject an artwork provider is asked about.
    ///
    /// A stored entity is described from its own row: an album by its title, its
    /// credited artists, and the identifiers the library recorded for it; an
    /// artist by its name and MusicBrainz id; a track by its album, or by its own
    /// title as an album when it has none. A catalog entity is described by what
    /// the catalog reports, which is cached by the catalog layer.
    ///
    /// # Parameters
    ///
    /// - `id`: the entity the picture is wanted for.
    ///
    /// # Returns
    ///
    /// The subject.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id names no entity, when it
    /// names a kind with no picture, and when a catalog lookup finds nothing, and
    /// the mapped provider failure when a catalog lookup fails.
    pub(crate) async fn subject(&self, id: &ClientId) -> Result<ArtworkSubject, CatalogError> {
        match id {
            ClientId::Entity(entity) => self.stored_subject(*entity).await,
            ClientId::Catalog(catalog) => match catalog.kind() {
                EntityKind::Album => {
                    let album = self.album(catalog).await?.album;
                    Ok(ArtworkSubject {
                        kind: EntityKind::Album,
                        provider_id: Some(catalog.provider_id().to_owned()),
                        ids: album.ids,
                        title: album.title,
                        artists: album
                            .artists
                            .iter()
                            .map(|credit| credit.name.clone())
                            .collect(),
                    })
                }
                EntityKind::Artist => {
                    let artist = self.artist(catalog).await?.artist;
                    Ok(ArtworkSubject {
                        kind: EntityKind::Artist,
                        provider_id: Some(catalog.provider_id().to_owned()),
                        ids: artist.ids,
                        title: artist.name,
                        artists: Vec::new(),
                    })
                }
                // A track has no picture of its own; the picture a client wants
                // for it is its album's.
                EntityKind::Track => {
                    let track = self.track(catalog).await?.track;
                    let provider_id = Some(catalog.provider_id().to_owned());
                    match track.album {
                        Some(album) => Ok(ArtworkSubject {
                            kind: EntityKind::Album,
                            provider_id,
                            ids: Vec::new(),
                            title: album.title,
                            artists: album
                                .artists
                                .iter()
                                .map(|credit| credit.name.clone())
                                .collect(),
                        }),
                        None => Ok(ArtworkSubject {
                            kind: EntityKind::Album,
                            provider_id,
                            ids: Vec::new(),
                            title: track.title,
                            artists: Vec::new(),
                        }),
                    }
                }
                _ => Err(CatalogError::NotFound),
            },
        }
    }

    /// Builds the subject of one stored album.
    ///
    /// # Parameters
    ///
    /// - `album`: the stored album.
    /// - `provider_id`: the issuing instance's own id for the entity, when the
    ///   server holds one.
    ///
    /// # Returns
    ///
    /// The subject.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the album does not exist, and
    /// [`CatalogError::Database`] when a read fails.
    async fn album_subject(
        &self,
        album: EntityId,
        provider_id: Option<String>,
    ) -> Result<ArtworkSubject, CatalogError> {
        let library = LibraryRepository::new(self.database());
        let Some(album) = library.get_album(album).await? else {
            return Err(CatalogError::NotFound);
        };
        let artists = library
            .list_album_artists(album.id)
            .await?
            .into_iter()
            .map(|artist| artist.name)
            .collect();
        Ok(ArtworkSubject {
            kind: EntityKind::Album,
            provider_id,
            ids: album_ids(&album),
            title: album.title,
            artists,
        })
    }

    /// Builds the subject of one stored entity.
    ///
    /// # Parameters
    ///
    /// - `entity`: the stored entity.
    ///
    /// # Returns
    ///
    /// The subject.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the entity does not exist, when
    /// its kind has no picture, and when a track has no album to describe.
    async fn stored_subject(&self, entity: EntityId) -> Result<ArtworkSubject, CatalogError> {
        let library = LibraryRepository::new(self.database());
        let provider_id = self
            .first_provider_alias(entity)
            .await?
            .map(|alias| alias.provider_id);
        match entity.kind() {
            IdPrefix::Album => self.album_subject(entity, provider_id).await,
            IdPrefix::Artist => {
                let Some(artist) = library.get_artist(entity).await? else {
                    return Err(CatalogError::NotFound);
                };
                Ok(ArtworkSubject {
                    kind: EntityKind::Artist,
                    provider_id,
                    ids: artist
                        .mb_artist_id
                        .filter(|id| !id.is_empty())
                        .map(|value| ExternalId {
                            namespace: MB_ARTIST.to_owned(),
                            value,
                        })
                        .into_iter()
                        .collect(),
                    title: artist.name,
                    artists: Vec::new(),
                })
            }
            IdPrefix::Track => {
                let Some(track) = library.get_track(entity).await? else {
                    return Err(CatalogError::NotFound);
                };
                match track.album_id {
                    Some(album) => self.album_subject(album, provider_id).await,
                    None => Ok(ArtworkSubject {
                        kind: EntityKind::Album,
                        provider_id,
                        ids: Vec::new(),
                        title: track.title,
                        artists: Vec::new(),
                    }),
                }
            }
            _ => Err(CatalogError::NotFound),
        }
    }

    /// Reads the images an instance answered for a subject.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that answered.
    /// - `key`: the subject's cache key.
    ///
    /// # Returns
    ///
    /// The images, or `None` when the cache does not hold them.
    async fn cached_images(&self, instance: Uuid, key: &RequestKey) -> Option<Vec<Image>> {
        match self.cache().get::<WireImages>(instance, key).await {
            Ok(Some(images)) => Some(
                images
                    .0
                    .into_iter()
                    .map(|image| Image {
                        url: image.url,
                        width: image.width,
                        height: image.height,
                    })
                    .collect(),
            ),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "a cached artwork answer could not be read");
                None
            }
        }
    }

    /// Stores the images an instance answered for a subject.
    ///
    /// A failure is logged and ignored: the caller already holds the answer, and
    /// cover art must not fail because its cache could not be written.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that answered.
    /// - `key`: the subject's cache key.
    /// - `images`: the images.
    /// - `ttl`: how long the answer stays fresh.
    async fn store_images(
        &self,
        instance: Uuid,
        key: &RequestKey,
        images: &[Image],
        ttl: Duration,
    ) {
        let wire = WireImages(
            images
                .iter()
                .map(|image| WireImage {
                    url: image.url.clone(),
                    width: image.width,
                    height: image.height,
                })
                .collect(),
        );
        self.cache().put(instance, key, &wire, ttl).await;
    }

    /// Returns whether an image URL was refused recently enough to skip.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance that described the image.
    /// - `url`: the image's URL.
    ///
    /// # Returns
    ///
    /// `true` when that instance's host refused this URL within
    /// [`REFUSED_IMAGE_TTL`], so the caller answers without asking again.
    async fn image_refused(&self, slug: &Slug, url: &str) -> bool {
        self.refused_images()
            .get(&refused_image_key(slug, url))
            .await
            .is_some()
    }

    /// Remembers that an image host refused a URL.
    ///
    /// Only a refusal the host itself stated is remembered. A network error or
    /// a timeout is not: this server's own rate limiter produces those, and
    /// they say nothing about the host.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance that described the image.
    /// - `url`: the image's URL.
    async fn refuse_image(&self, slug: &Slug, url: &str) {
        self.refused_images()
            .insert(refused_image_key(slug, url), ())
            .await;
    }

    /// Returns whether an image host recently answered a URL as missing.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance that described the image.
    /// - `url`: the image's URL.
    ///
    /// # Returns
    ///
    /// `true` when the host answered 403, 404 or 410 for it within
    /// [`REFUSED_IMAGE_TTL`].
    async fn image_missing(&self, slug: &Slug, url: &str) -> bool {
        self.missing_images()
            .get(&missing_image_key(slug, url))
            .await
            .is_some()
    }

    /// Remembers that an image host answered a URL as missing.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance that described the image.
    /// - `url`: the image's URL.
    async fn mark_image_missing(&self, slug: &Slug, url: &str) {
        self.missing_images()
            .insert(missing_image_key(slug, url), ())
            .await;
    }

    /// Fetches one image an artwork instance described.
    ///
    /// The fetch goes through the instance's own HTTP grant, so an image host the
    /// instance may not reach is denied here too, and the bytes are stored when
    /// that instance's manifest allows it.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that described the image.
    /// - `kind`: the kind of entity the picture is for.
    /// - `subject`: the subject the image was found for.
    /// - `image`: the image to fetch.
    ///
    /// # Returns
    ///
    /// The image's MIME type and its bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the URL answers with a not-found,
    /// [`CatalogError::Unavailable`] when the instance cannot deliver the image
    /// right now, and [`CatalogError::Internal`] when the instance may not reach
    /// the URL at all.
    async fn fetch_layer_image(
        &self,
        instance: &RegistryInstance,
        kind: EntityKind,
        subject: &ArtworkSubject,
        image: &Image,
    ) -> Result<CoverArt, CatalogError> {
        let slug = instance.slug().clone();
        let key = layer_cache_key(&slug, kind, subject, image);
        if let Some(cached) = self.cached_cover(&key).await {
            return Ok(cached);
        }

        if self.image_missing(&slug, &image.url).await {
            tracing::debug!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                "an image host has no image at this URL; not asking again yet"
            );
            return Err(CatalogError::NotFound);
        }

        if self.image_refused(&slug, &image.url).await {
            tracing::debug!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                "an image host refused this URL recently; not asking again yet"
            );
            return Err(CatalogError::Unavailable {
                instance: slug.as_str().to_owned(),
            });
        }

        let fetched = self
            .registry()
            .fetch_image(slug.as_str(), &image.url)
            .await
            .map_err(|error| fetch_failure(&slug, &image.url, error))?;
        if is_missing_image(fetched.status) {
            tracing::debug!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                status = fetched.status,
                "an artwork instance has no image at that URL"
            );
            self.mark_image_missing(&slug, &image.url).await;
            return Err(CatalogError::NotFound);
        }
        if !(200..300).contains(&fetched.status) {
            tracing::warn!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                status = fetched.status,
                "an artwork instance answered an image request with a failure"
            );
            self.refuse_image(&slug, &image.url).await;
            return Err(CatalogError::Unavailable {
                instance: slug.as_str().to_owned(),
            });
        }

        let mime_type = fetched
            .content_type
            .clone()
            .unwrap_or_else(|| mime_for(image_url_name(&image.url)));
        if fetched.cache_policy == ImageCachePolicy::Cache {
            self.store_cover(&key, &slug, image, &mime_type, &fetched.bytes)
                .await;
        }
        Ok(CoverArt {
            mime_type,
            bytes: fetched.bytes,
        })
    }

    /// Reads a stored cover image.
    ///
    /// # Parameters
    ///
    /// - `key`: the cache key of the image.
    ///
    /// # Returns
    ///
    /// The stored image, or `None` when the cache does not hold it. A row whose
    /// file is gone, and a file that cannot be read, are both a miss: the caller
    /// fetches the image again rather than failing.
    async fn cached_cover(&self, key: &str) -> Option<CoverArt> {
        let row = sqlx::query!(
            "SELECT path, content_type FROM artwork_cache WHERE key = ?",
            key,
        )
        .fetch_optional(self.database().readers())
        .await
        .ok()
        .flatten()?;
        match tokio::fs::read(self.artwork_dir().join(&row.path)).await {
            Ok(bytes) => Some(CoverArt {
                mime_type: row.content_type,
                bytes,
            }),
            Err(error) => {
                tracing::warn!(
                    key,
                    path = row.path.as_str(),
                    error = %error,
                    "a cached cover image could not be read; fetching it again"
                );
                None
            }
        }
    }

    /// Writes a cover image to the artwork cache.
    ///
    /// A failure is logged and ignored: the caller already holds the bytes, and
    /// cover art must not fail because its cache could not be written.
    ///
    /// # Parameters
    ///
    /// - `key`: the cache key of the image.
    /// - `slug`: the instance that issued the entity.
    /// - `image`: the image the provider described.
    /// - `mime_type`: the MIME type of the bytes.
    /// - `bytes`: the encoded image.
    async fn store_cover(
        &self,
        key: &str,
        slug: &Slug,
        image: &Image,
        mime_type: &str,
        bytes: &[u8],
    ) {
        let name = format!(
            "{}.{}",
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, key.as_bytes()),
            extension_for(mime_type)
        );
        let path = self.artwork_dir().join(&name);
        if let Err(error) = write_file(&path, bytes).await {
            tracing::warn!(
                key,
                path = %path.display(),
                error = %error,
                "a cover image could not be cached"
            );
            return;
        }

        let written = sqlx::query!(
            "INSERT INTO artwork_cache (key, source, path, content_type, width, height, \
                                        fetched_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (key) DO UPDATE SET \
                source = excluded.source, \
                path = excluded.path, \
                content_type = excluded.content_type, \
                width = excluded.width, \
                height = excluded.height, \
                fetched_at = excluded.fetched_at",
            key,
            image.url,
            name,
            mime_type,
            image.width,
            image.height,
            crate::cache::now_millis(),
        )
        .execute(self.database().writer())
        .await;
        if let Err(error) = written {
            tracing::warn!(
                instance = slug.as_str(),
                key,
                error = %error,
                "a cached cover image was not indexed"
            );
        }
    }
}

/// Writes a file, creating its directory first.
///
/// # Parameters
///
/// - `path`: the absolute path to write.
/// - `bytes`: the bytes to write.
///
/// # Errors
///
/// Returns the filesystem error of creating the directory or writing the file.
async fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, bytes).await
}

/// Picks the image to serve for a requested size.
///
/// The size is a maximum, so the smallest image the provider describes as at
/// least that large is the one that wastes the least bandwidth without being
/// blurry; when the provider has nothing that large, the largest it has is
/// served rather than nothing. A provider that describes no dimensions at all
/// leaves no way to choose, and the first image it lists is served.
///
/// # Parameters
///
/// - `images`: the images the provider described, in its order.
/// - `size`: the requested size in pixels, when the caller named one.
///
/// # Returns
///
/// The image to serve, or `None` when the provider described none.
fn pick_image(images: &[Image], size: Option<u32>) -> Option<&Image> {
    images.first()?;
    if let Some(size) = size {
        let smallest_sufficient = images
            .iter()
            .filter(|image| dimension(image).is_some_and(|pixels| pixels >= size))
            .min_by_key(|image| dimension(image).unwrap_or(u32::MAX));
        if let Some(image) = smallest_sufficient {
            return Some(image);
        }
    }
    images.iter().fold(None, |largest, image| match largest {
        None => Some(image),
        Some(current) => match (dimension(image), dimension(current)) {
            (Some(new), Some(old)) if new > old => Some(image),
            (Some(_), None) => Some(image),
            _ => largest,
        },
    })
}

/// Returns the largest side of an image, when the provider describes one.
///
/// # Parameters
///
/// - `image`: the image.
fn dimension(image: &Image) -> Option<u32> {
    image.width.max(image.height)
}

/// Returns whether an image fetch's status means the image does not exist.
///
/// A refusal (403), a not-found (404) and a gone resource (410) are all
/// permanent for the URL: the provider described a picture it cannot deliver,
/// and asking again will not bring it back. They are reported as
/// [`CatalogError::NotFound`] rather than as transient unavailability, so a
/// caller tries the next source or falls back to local art instead of telling
/// the client to retry a request that will fail the same way.
///
/// # Parameters
///
/// - `status`: the HTTP status the image host answered with.
///
/// # Returns
///
/// `true` for 403, 404 and 410.
fn is_missing_image(status: u16) -> bool {
    matches!(status, 403 | 404 | 410)
}

/// Returns the missing-image window's key for an image URL.
///
/// The instance is part of the key for the same reason the refusal key names
/// it: two instances may describe one URL under different grants.
///
/// # Parameters
///
/// - `slug`: the instance that described the image.
/// - `url`: the image's URL.
fn missing_image_key(slug: &Slug, url: &str) -> String {
    format!("missing {} {url}", slug.as_str())
}

/// Returns the refusal window's key for an image URL.
///
/// The instance is part of the key because two instances may describe the same
/// URL under different grants, and only the instance whose host refused it
/// should skip it.
///
/// # Parameters
///
/// - `slug`: the instance that described the image.
/// - `url`: the image's URL.
fn refused_image_key(slug: &Slug, url: &str) -> String {
    format!("{} {url}", slug.as_str())
}

/// Builds the cache key of one catalog cover image.
///
/// The key names the entity and the image the provider described for it, so two
/// entities, two instances and two sizes of one image never share a row. The
/// image's dimensions are part of it because a provider may serve one URL for
/// every size it describes.
///
/// # Parameters
///
/// - `slug`: the instance that issued the entity.
/// - `kind`: the entity kind.
/// - `provider_id`: the provider's own id for the entity.
/// - `image`: the image the provider described.
fn cache_key(slug: &Slug, kind: EntityKind, provider_id: &str, image: &Image) -> String {
    let size = match dimension(image) {
        Some(pixels) => pixels.to_string(),
        None => "-".to_owned(),
    };
    format!(
        "catalog|{}|{}|{provider_id}|{size}|{}",
        slug.as_str(),
        kind_literal(kind),
        image.url
    )
}

/// Returns the file name a URL ends in, with any query or fragment removed.
///
/// # Parameters
///
/// - `url`: the image URL.
fn image_url_name(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// Returns the file extension for a MIME type.
///
/// # Parameters
///
/// - `mime_type`: the image's MIME type.
fn extension_for(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "img",
    }
}

/// Maps a failed image fetch into a catalog error.
///
/// # Parameters
///
/// - `slug`: the instance the fetch went through.
/// - `url`: the image URL.
/// - `error`: the failure the fetch reported.
fn fetch_failure(slug: &Slug, url: &str, error: FetchError) -> CatalogError {
    match error {
        FetchError::UnknownInstance(_) => CatalogError::NotFound,
        FetchError::Http(HttpError::Denied(reason)) => {
            tracing::warn!(
                instance = slug.as_str(),
                url,
                reason,
                "the catalog instance is not allowed to reach its own image host"
            );
            CatalogError::Internal {
                instance: slug.as_str().to_owned(),
            }
        }
        FetchError::Http(error) => {
            tracing::warn!(
                instance = slug.as_str(),
                url,
                error = %error,
                "a catalog image could not be fetched"
            );
            CatalogError::Unavailable {
                instance: slug.as_str().to_owned(),
            }
        }
    }
}

/// Reads `artwork.timeout_ms` from a snapshot.
///
/// # Parameters
///
/// - `settings`: the settings in effect.
///
/// # Returns
///
/// The deadline for one artwork lookup.
///
/// # Errors
///
/// Returns [`CatalogError::Setting`] when the snapshot no longer holds a valid
/// value for the key.
fn artwork_timeout(settings: &hearthsound_config::SettingsStore) -> Result<Duration, CatalogError> {
    let millis = settings.get(hearthsound_config::keys::ARTWORK_TIMEOUT_MS)?;
    Ok(Duration::from_millis(u64::try_from(millis).unwrap_or(0)))
}

/// Returns the identifiers a stored album recorded.
///
/// # Parameters
///
/// - `album`: the album row.
///
/// # Returns
///
/// One identifier per canonical namespace the album carries a value for.
fn album_ids(album: &hearthsound_library::Album) -> Vec<ExternalId> {
    let mut ids = Vec::new();
    for (namespace, value) in [
        (MB_RELEASE, album.mb_release_id.as_deref()),
        (MB_RELEASE_GROUP, album.mb_release_group_id.as_deref()),
        (UPC, album.upc.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            ids.push(ExternalId {
                namespace: namespace.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    ids
}

/// Returns the cache key of one image an artwork instance described.
///
/// The key names the instance, the subject, and the image, so two instances that
/// describe the same URL keep their own copies under their own cache policies.
///
/// # Parameters
///
/// - `slug`: the instance that described the image.
/// - `kind`: the kind of entity the picture is for.
/// - `subject`: the subject the image was found for.
/// - `image`: the image.
///
/// # Returns
///
/// The key.
fn layer_cache_key(
    slug: &Slug,
    kind: EntityKind,
    subject: &ArtworkSubject,
    image: &Image,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    subject.title.hash(&mut hash);
    subject.artists.hash(&mut hash);
    for id in &subject.ids {
        id.namespace.hash(&mut hash);
        id.value.hash(&mut hash);
    }
    format!(
        "artwork|{}|{}|{:x}|{}x{}",
        slug.as_str(),
        kind_literal(kind),
        hash.finish(),
        image.width.unwrap_or(0),
        image.height.unwrap_or(0),
    )
}

/// Returns the literal one entity kind is written as in a cache key.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an image for the selection tests.
    fn image(url: &str, width: Option<u32>, height: Option<u32>) -> Image {
        Image {
            url: url.to_owned(),
            width,
            height,
        }
    }

    #[test]
    fn the_smallest_image_that_covers_the_request_is_served() {
        let images = [
            image("small", Some(250), Some(250)),
            image("medium", Some(500), Some(500)),
            image("large", Some(1_000), Some(1_000)),
        ];

        assert_eq!(pick_image(&images, Some(300)).unwrap().url, "medium");
        assert_eq!(pick_image(&images, Some(250)).unwrap().url, "small");
        assert_eq!(pick_image(&images, Some(1_000)).unwrap().url, "large");
    }

    #[test]
    fn the_largest_image_is_served_when_none_is_big_enough() {
        let images = [
            image("small", Some(250), Some(250)),
            image("large", Some(1_000), Some(1_000)),
        ];

        assert_eq!(pick_image(&images, Some(2_000)).unwrap().url, "large");
    }

    #[test]
    fn the_largest_image_is_served_when_no_size_is_asked_for() {
        let images = [
            image("large", Some(1_000), Some(1_000)),
            image("small", Some(250), Some(250)),
        ];

        assert_eq!(pick_image(&images, None).unwrap().url, "large");
    }

    #[test]
    fn an_image_without_dimensions_ranks_below_one_that_has_them() {
        let images = [
            image("described", None, None),
            image("measured", Some(600), Some(600)),
        ];

        assert_eq!(pick_image(&images, None).unwrap().url, "measured");
        assert_eq!(pick_image(&images, Some(100)).unwrap().url, "measured");
    }

    #[test]
    fn a_provider_that_describes_no_dimensions_leaves_the_first_image() {
        let images = [image("first", None, None), image("second", None, None)];

        assert_eq!(pick_image(&images, None).unwrap().url, "first");
        assert_eq!(pick_image(&images, Some(500)).unwrap().url, "first");
    }

    #[test]
    fn an_entity_without_images_has_no_cover() {
        assert!(pick_image(&[], Some(500)).is_none());
        assert!(pick_image(&[], None).is_none());
    }

    #[test]
    fn the_cache_key_names_the_entity_and_the_image() {
        let slug = Slug::new("deezer").unwrap();
        let cover = image("https://cdn.example.com/cover.jpg", Some(500), Some(500));

        let key = cache_key(&slug, EntityKind::Album, "3135556", &cover);

        assert_eq!(
            key,
            "catalog|deezer|album|3135556|500|https://cdn.example.com/cover.jpg"
        );
        assert_ne!(
            key,
            cache_key(&slug, EntityKind::Track, "3135556", &cover),
            "two entity kinds never share a row"
        );
    }

    #[test]
    fn a_file_name_is_derived_from_the_mime_type() {
        assert_eq!(extension_for("image/jpeg"), "jpg");
        assert_eq!(extension_for("image/png"), "png");
        assert_eq!(extension_for("image/avif"), "img");
    }

    #[test]
    fn a_url_name_loses_its_query_and_fragment() {
        assert_eq!(
            image_url_name("https://cdn/cover.jpg?x=1"),
            "https://cdn/cover.jpg"
        );
        assert_eq!(
            image_url_name("https://cdn/cover.png#frag"),
            "https://cdn/cover.png"
        );
    }
}

#[cfg(test)]
mod missing_image_tests {
    use super::is_missing_image;

    #[test]
    fn a_refused_or_gone_image_is_missing() {
        assert!(is_missing_image(403));
        assert!(is_missing_image(404));
        assert!(is_missing_image(410));
    }

    #[test]
    fn a_server_failure_stays_transient() {
        assert!(!is_missing_image(500));
        assert!(!is_missing_image(503));
        assert!(!is_missing_image(200));
    }
}
