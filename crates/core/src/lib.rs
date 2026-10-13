//! Core domain types shared across the Hearthsound server.
//!
//! This crate holds the vocabulary every other crate speaks: the [`Layer`] enum
//! naming every pluggable behavior, [`EntityKind`] naming the catalog entities,
//! the shared [`ProviderError`], the [`Slug`] identifier used for provider
//! instances, the client id codec in [`id`], and the provider traits and
//! WebAssembly interface records in [`providers`]. It deliberately depends on
//! almost nothing so every other crate can depend on it without pulling in a
//! runtime.
#![forbid(unsafe_code)]

mod error;
mod id;
mod kind;
mod layer;
mod slug;

pub mod providers;

pub use error::ProviderError;
pub use id::{CatalogId, ClientId, EntityId, IdError, IdPrefix, new_uuid_v7};
pub use kind::EntityKind;
pub use layer::{Layer, UnknownLayer};
pub use slug::{InvalidSlug, Slug};
