//! The shared provider [`ProviderError`].

/// The error type shared by every provider call.
///
/// It mirrors the WebAssembly `provider-error` variant one-for-one, so a
/// plugin's error crosses the host boundary without loss. It intentionally
/// carries no free-form message: [`ProviderError::RateLimited`] is the only
/// variant with a payload, because a retry delay is actionable to the caller.
///
/// Retry policy is decided by the caller: `RateLimited` and `Unavailable` are
/// retried with jittered exponential backoff, while `NotFound`, `Unauthorized`
/// and `InvalidConfig` are terminal. `Internal` means the plugin misbehaved (a
/// trap), and is handled by the plugin host rather than retried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The requested entity does not exist.
    #[error("not found")]
    NotFound,
    /// The provider asked the caller to slow down.
    #[error("rate limited; retry after {retry_after_ms} ms")]
    RateLimited {
        /// The minimum delay before the next attempt, in milliseconds.
        retry_after_ms: u64,
    },
    /// The configured credentials were rejected.
    #[error("unauthorized")]
    Unauthorized,
    /// The provider configuration is invalid.
    #[error("invalid configuration")]
    InvalidConfig,
    /// The provider is temporarily unable to serve requests.
    #[error("unavailable")]
    Unavailable,
    /// The provider failed internally.
    #[error("internal provider error")]
    Internal,
}
