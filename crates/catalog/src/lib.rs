//! The catalog layer: provider lookups, caching, aliasing and materialization.
//!
//! The server can play a song it does not store. Behind that behavior sits this
//! crate: it turns a client's catalog id into provider calls, keeps the answers
//! a provider gave so a search does not spend quota twice, and writes the rows
//! that turn an external entity into a stored one.
//!
//! [`CatalogService`] is the only entry point. It is built from the provider
//! registry, the database and the settings service, and it never sees
//! WebAssembly: the registry hands it core trait objects, and this crate only
//! ever calls those.
//!
//! The four rules the design fixes, and where they live:
//!
//! - the catalog layer has one primary instance for search and lookups; later
//!   instances are tried only when the primary errors or times out, and their
//!   results are never merged ([`CatalogService::search`]);
//! - every result carries the [`CatalogId`] that names it, which is the
//!   provider instance slug plus the provider's own id, so a client can stream,
//!   save or acquire the entity later;
//! - an unmaterialized entity is materialized through the `external_ids`
//!   aliases, which keep the original id resolving forever and which let two
//!   providers that know the same recording agree on one row;
//! - these tables are this crate's: `external_ids` and `catalog_cache`.
//!
//! # Examples
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use hearthsound_catalog::CatalogService;
//! use hearthsound_core::providers::CatalogQuery;
//! use hearthsound_core::EntityKind;
//!
//! # async fn example(
//! #     registry: Arc<hearthsound_plugin_host::ProviderRegistry>,
//! #     database: Arc<hearthsound_db::Database>,
//! #     settings: hearthsound_config::settings::Settings,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! let catalog = CatalogService::new(registry, database, settings, "/srv/hearthsound/artwork");
//! let results = catalog
//!     .search(&CatalogQuery {
//!         text: "Energy 52".to_owned(),
//!         kinds: vec![EntityKind::Track],
//!         limit: 10,
//!         offset: 0,
//!     })
//!     .await?;
//! for track in results.tracks {
//!     println!("{} {}", track.id, track.track.title);
//! }
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

pub mod aliases;
pub mod artist_info;
pub mod artwork;
pub mod cache;
pub mod dedupe;
pub mod discovery;
mod error;
mod identity;
mod materialize;
pub mod pages;
pub mod paths;
pub mod relevance;
mod service;
mod wire;

pub use artwork::{CoverArt, CoverArtRequest};
pub use cache::{ENTITY_CACHE_TTL, RequestKey, SEARCH_CACHE_TTL};
pub use dedupe::{DURATION_TOLERANCE_MS, DedupedCatalog, LocalTrackHit, MaterializedEntity};
pub use discovery::{
    DiscoveryService, InstancePlaylist, LinkedUserContexts, NoUserContexts, ResolvedAlbum,
    ResolvedArtist, SimilarArtistAlbums, UserContextSource,
};
pub use error::CatalogError;
pub use identity::{IDENTITY_MATCH_THRESHOLD, IdentityRequest, ResolvedRecording};
pub use materialize::{MaterializedAlbum, ProviderAlias, materialize_entity};
pub use pages::{
    AlbumHead, AlbumPage, ArtistHead, ArtistPage, MAX_PAGE_ALBUMS, PageAlbum, PageDepth, PageTrack,
};
pub use paths::ResolvedPath;
pub use service::{
    CatalogAlbumRefResult, CatalogAlbumResult, CatalogArtistResult, CatalogSearch, CatalogService,
    CatalogTrackResult,
};
