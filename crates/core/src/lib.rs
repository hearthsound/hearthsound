//! Core domain types shared across the Hearthsound server.
//!
//! This crate holds the vocabulary every other crate speaks: the [`Layer`] enum
//! naming every pluggable behavior, [`EntityKind`] naming the catalog entities,
//! the shared [`ProviderError`], the [`Slug`] identifier used for provider
//! instances, the client id codec in [`id`], the availability, acquisition,
//! quality and storage literals, the provider traits and WebAssembly interface
//! records in [`providers`], and the server event vocabulary and bus in
//! [`events`]. It deliberately depends on almost nothing so every other crate
//! can depend on it without pulling in a runtime.
#![forbid(unsafe_code)]

mod acquisition;
mod availability;
mod error;
mod id;
mod kind;
mod layer;
mod quality;
mod slug;
mod storage;

pub mod events;
pub mod providers;

pub use acquisition::{
    AcquisitionState, FailureReason, Granularity, ParkReason, Priority, UnknownAcquisitionState,
    UnknownFailureReason, UnknownGranularity, UnknownParkReason, UnknownPriority,
};
pub use availability::{Availability, UnknownAvailability};
pub use error::ProviderError;
pub use id::{EntityId, IdError, IdPrefix, new_uuid_v7};
pub use kind::EntityKind;
pub use layer::{Layer, UnknownLayer};
pub use quality::{QualityTier, UnknownQualityTier};
pub use slug::{InvalidSlug, Slug};
pub use storage::{
    ImportMode, PinReason, StorageClass, UnknownImportMode, UnknownPinReason, UnknownStorageClass,
};
