//! Core domain types shared across the Hearthsound server.
//!
//! This crate holds the provider abstraction's vocabulary: the [`Layer`] enum
//! naming every pluggable behavior, [`EntityKind`] naming the catalog entities,
//! the shared [`ProviderError`], and the [`Slug`] identifier used for provider
//! instances. It deliberately depends on almost nothing so every other crate
//! can depend on it without pulling in a runtime.
#![forbid(unsafe_code)]

mod error;
mod kind;
mod layer;
mod slug;

pub use error::ProviderError;
pub use kind::EntityKind;
pub use layer::{Layer, UnknownLayer};
pub use slug::{InvalidSlug, Slug};
