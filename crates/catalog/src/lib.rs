//! The entity layer: one row per artist, album and track, whoever named it.
//!
//! Every artist, album and track the server sees - scanned, uploaded,
//! downloaded, found by a search, listed in a discography, suggested by
//! discovery - is one row, and every provider's identifier for it is a row of
//! `external_ids`. This crate is what turns what the providers say into those
//! rows and what recomputes the rows' fields from everything stored:
//!
//! - [`resolve`] is the one identity rule, and every provider record enters the
//!   database through it;
//! - [`index`] walks the queue and stores what each instance answered, at the
//!   depth the queue asked for;
//! - [`fields`] recomputes an entity's fields, credits and list relations from
//!   its file tags and its stored provider answers, following the administrator's
//!   precedence and layer order;
//! - [`images`] serves every picture from the server's own store;
//! - [`views`] composes the pages both APIs render;
//! - [`search`] answers with stored rows, the library first and the catalog
//!   second;
//! - [`discovery`] resolves suggestions into rows and stores its answers.
//!
//! [`CatalogService`] is the handle the rest of the server holds.
//!
//! # Examples
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use hearthsound_catalog::CatalogService;
//! use hearthsound_core::EntityKind;
//!
//! # async fn example(
//! #     registry: Arc<hearthsound_plugin_host::ProviderRegistry>,
//! #     database: Arc<hearthsound_db::Database>,
//! #     settings: Arc<hearthsound_config::SettingsStore>,
//! #     ffmpeg: Arc<hearthsound_media::FfmpegRunner>,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! let catalog = CatalogService::new(registry, database, settings, "/srv/artwork", ffmpeg);
//! let answer = catalog
//!     .search(Uuid::nil(), "Energy 52", &[EntityKind::Track], 10, 0)
//!     .await?;
//! println!("{} tracks", answer.tracks.len());
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

pub mod discovery;
mod error;
pub mod fields;
mod held;
pub mod identity;
pub mod images;
pub mod index;
pub mod normalize;
pub mod relevance;
pub mod resolve;
pub mod search;
mod service;
pub mod sources;
pub mod views;
pub mod wire;

pub use discovery::{
    DiscoveryService, InstancePlaylist, LinkedUserContexts, NoUserContexts, UserContextSource,
};
pub use error::CatalogError;
pub use fields::{LayerOrder, Precedence};
pub use identity::{IDENTITY_MATCH_THRESHOLD, IdentityRequest, IdentityService, ResolvedRecording};
pub use images::{ImageService, ServedImage};
pub use index::{Completeness, Indexer};
pub use normalize::{DURATION_TOLERANCE_MS, base_title, normalize};
pub use resolve::EntityResolver;
pub use search::{ANSWER_TTL, SearchAnswer, SearchService};
pub use service::CatalogService;
pub use sources::{Operation, PAYLOAD_VERSION, Source, Status};
pub use views::{AlbumRef, AlbumView, ArtistRef, ArtistView, EntityViews, Link, TrackRef, Viewer};
