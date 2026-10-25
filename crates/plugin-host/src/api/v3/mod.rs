//! Frozen adapter for `hearthsound:plugin@0.3.0`. Add nothing here. Port only
//! bug fixes that affect every API version. Delete this tree when 0.3 support is
//! dropped.
//!
//! The tree is a copy of the adapters as they shipped in core v0.1.6, when
//! 0.3.0 was the newest package, with only their paths rewritten: the bindings
//! read `wit/since_v0.3/` and the cross-adapter helpers are addressed inside
//! this tree. Anything the shared modules enforce - permissions, deadlines,
//! trap accounting, the process cap - applies here through the same code the
//! newer versions use.

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

use crate::api::v3::bindings::hearthsound::plugin::types::ProviderError as WitProviderError;
use crate::error::GuestError;

impl From<WitProviderError> for GuestError {
    /// Converts the 0.3.0 `provider-error` variant to the version-neutral one.
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
