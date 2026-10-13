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

use hearthsound_core::providers::Image;
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, Slug};
use hearthsound_library::artwork::mime_for;
use hearthsound_plugin_host::FetchError;
use hearthsound_plugin_host::http::HttpError;
use hearthsound_plugin_host::manifest::ImageCachePolicy;

use crate::error::CatalogError;
use crate::materialize::ProviderAlias;
use crate::service::CatalogService;

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

impl CatalogService {
    /// Serves the cover art of a catalog entity.
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
    pub async fn cover_art(&self, request: &CoverArtRequest) -> Result<CoverArt, CatalogError> {
        let (slug, kind, provider_id) = self.issuing_instance(&request.id).await?;
        let images = self.entity_images(&slug, kind, &provider_id).await?;
        let image = pick_image(&images, request.size).ok_or(CatalogError::NotFound)?;
        let key = cache_key(&slug, kind, &provider_id, image);

        if let Some(cached) = self.cached_cover(&key).await {
            return Ok(cached);
        }

        let fetched = self
            .registry()
            .fetch_image(slug.as_str(), &image.url)
            .await
            .map_err(|error| fetch_failure(&slug, &image.url, error))?;
        if fetched.status == 404 {
            tracing::debug!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                "the catalog instance has no image at that URL"
            );
            return Err(CatalogError::NotFound);
        }
        if !(200..300).contains(&fetched.status) {
            tracing::warn!(
                instance = slug.as_str(),
                url = image.url.as_str(),
                status = fetched.status,
                "the catalog instance answered an image request with a failure"
            );
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

/// Returns the literal an entity kind has in a cache key.
///
/// # Parameters
///
/// - `kind`: the entity kind.
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
