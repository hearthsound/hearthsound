//! Adapters for `hearthsound:plugin@0.4.0`, the frozen previous plugin API.
//!
//! The bindings here read `wit/since_v0.4/`, the package the host still loads
//! for plugins that have not moved to 0.5.0. Each module converts one
//! interface's WIT types to core types and back; the host imports the guest
//! calls live in [`state`].

pub(crate) mod account_link;
pub(crate) mod acquisition;
pub(crate) mod artwork;
pub(crate) mod bindings;
pub(crate) mod catalog;
pub(crate) mod discovery;
pub(crate) mod metadata;
pub(crate) mod scrobble;
pub(crate) mod state;
pub(crate) mod verification;

use crate::api::v4::bindings::hearthsound::plugin::types::ProviderError as WitProviderError;
use crate::error::GuestError;

impl From<WitProviderError> for GuestError {
    /// Converts the 0.4.0 `provider-error` variant to the version-neutral one.
    ///
    /// # Parameters
    ///
    /// - `error`: the error the guest returned.
    fn from(error: WitProviderError) -> GuestError {
        match error {
            WitProviderError::NotFound => GuestError::NotFound,
            WitProviderError::RateLimited(millis) => GuestError::RateLimited(millis),
            WitProviderError::Unauthorized(message) => GuestError::Unauthorized(message),
            WitProviderError::InvalidConfig(message) => GuestError::InvalidConfig(message),
            WitProviderError::Unavailable(message) => GuestError::Unavailable(message),
            WitProviderError::Internal(message) => GuestError::Internal(message),
        }
    }
}
