//! The error type of the catalog layer.
//!
//! Every provider failure is translated into a [`CatalogError`] that keeps the
//! three distinctions the API layer has to render: an entity that does not
//! exist is a not-found, an instance that cannot answer right now is an
//! unavailable, and an instance that asked the caller to slow down carries the
//! delay it asked for. The remaining provider variants (`Unauthorized`,
//! `InvalidConfig`, `Internal`) are mapped one for one, because an administrator
//! needs to tell a rejected credential from a misconfigured instance.

use hearthsound_core::ProviderError;

/// An error from the catalog layer.
///
/// The variant names the API layer maps onto Subsonic and native error codes.
/// Every variant that describes a provider failure carries the slug of the
/// instance it came from, so a log line or an operator-facing message names the
/// instance and not just the failure.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The entity does not exist, or the id names an instance that does not
    /// serve this layer.
    ///
    /// A lookup routed to an unknown, disabled, unassigned or catalog-less
    /// instance is reported as not found, never as a server error: the entity
    /// the caller named is not reachable through this server, which is what a
    /// not-found means to a client.
    #[error("the catalog entity does not exist")]
    NotFound,
    /// The instance could not serve the request right now.
    ///
    /// This is the mapped [`ProviderError::Unavailable`] and also the result of
    /// a lookup that exceeded its deadline.
    #[error("catalog instance `{instance}` is unavailable")]
    Unavailable {
        /// The slug of the instance that failed.
        instance: String,
    },
    /// The instance asked the caller to slow down.
    #[error("catalog instance `{instance}` is rate limited; retry after {retry_after_ms} ms")]
    RateLimited {
        /// The slug of the instance that rate limited the caller.
        instance: String,
        /// The minimum delay before the next attempt, in milliseconds.
        retry_after_ms: u64,
    },
    /// The instance rejected the credentials the server configured.
    #[error("catalog instance `{instance}` rejected its credentials")]
    Unauthorized {
        /// The slug of the instance that rejected its credentials.
        instance: String,
    },
    /// The instance's configuration is invalid.
    #[error("catalog instance `{instance}` has an invalid configuration")]
    InvalidConfig {
        /// The slug of the instance that is misconfigured.
        instance: String,
    },
    /// The instance failed internally, for example by trapping.
    #[error("catalog instance `{instance}` failed internally")]
    Internal {
        /// The slug of the instance that failed.
        instance: String,
    },
    /// A provider returned an entity that cannot be named by a client id.
    ///
    /// The only way this happens is a provider whose entity id is empty, which
    /// is a contract violation of the catalog interface.
    #[error("catalog instance `{instance}` returned an entity without an id")]
    InvalidEntity {
        /// The slug of the instance that returned the entity.
        instance: String,
    },
    /// A stored identifier is not a UUID.
    ///
    /// Every id the catalog reads is a 16-byte UUIDv7, so this means the row was
    /// written by something other than this server.
    #[error("a stored id is not a UUID")]
    CorruptId,
    /// The library layer rejected a row the catalog layer writes.
    ///
    /// Materialization stores artists, albums and tracks through the library's
    /// own writers, so their failures surface here unchanged.
    #[error(transparent)]
    Library(#[from] hearthsound_library::LibraryError),
    /// A setting could not be read.
    #[error(transparent)]
    Setting(#[from] hearthsound_config::ConfigError),
    /// A value could not be encoded as JSON.
    ///
    /// The alias lookups send a page of values as a JSON array, so this is a
    /// failure of encoding a request, never of decoding a provider's answer.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// The database rejected a read or a write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The job queue refused a job.
    #[error("job queue error: {0}")]
    Queue(#[from] hearthsound_jobs::JobsError),
}

impl CatalogError {
    /// Maps a provider failure from one instance into a catalog error.
    ///
    /// # Parameters
    ///
    /// - `instance`: the slug of the instance the call went to.
    /// - `error`: the failure the provider reported.
    ///
    /// # Returns
    ///
    /// The matching catalog error, carrying the instance slug.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::CatalogError;
    /// use hearthsound_core::ProviderError;
    ///
    /// let mapped = CatalogError::from_provider("deezer", ProviderError::NotFound);
    /// assert!(matches!(mapped, CatalogError::NotFound));
    /// ```
    pub fn from_provider(instance: &str, error: ProviderError) -> CatalogError {
        match error {
            ProviderError::NotFound => CatalogError::NotFound,
            ProviderError::RateLimited { retry_after_ms } => CatalogError::RateLimited {
                instance: instance.to_owned(),
                retry_after_ms,
            },
            ProviderError::Unauthorized => CatalogError::Unauthorized {
                instance: instance.to_owned(),
            },
            ProviderError::InvalidConfig => CatalogError::InvalidConfig {
                instance: instance.to_owned(),
            },
            ProviderError::Unavailable => CatalogError::Unavailable {
                instance: instance.to_owned(),
            },
            ProviderError::Internal => CatalogError::Internal {
                instance: instance.to_owned(),
            },
        }
    }

    /// Returns the slug of the instance the error came from, when it names one.
    ///
    /// # Returns
    ///
    /// The instance slug for every provider failure, and `None` for
    /// [`CatalogError::NotFound`] and the local failures, which no single
    /// instance owns.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::CatalogError;
    ///
    /// let error = CatalogError::from_provider("deezer", hearthsound_core::ProviderError::Internal);
    /// assert_eq!(error.instance(), Some("deezer"));
    /// assert_eq!(CatalogError::NotFound.instance(), None);
    /// ```
    pub fn instance(&self) -> Option<&str> {
        match self {
            CatalogError::NotFound
            | CatalogError::CorruptId
            | CatalogError::Library(_)
            | CatalogError::Setting(_)
            | CatalogError::Json(_)
            | CatalogError::Queue(_)
            | CatalogError::Database(_) => None,
            CatalogError::Unavailable { instance }
            | CatalogError::RateLimited { instance, .. }
            | CatalogError::Unauthorized { instance }
            | CatalogError::InvalidConfig { instance }
            | CatalogError::Internal { instance }
            | CatalogError::InvalidEntity { instance } => Some(instance),
        }
    }

    /// Returns whether the failure is transient.
    ///
    /// A transient failure is worth retrying: the instance was unavailable, or
    /// rate limited the caller and said when to come back. Everything else -
    /// not found, rejected credentials, invalid configuration, a misbehaving
    /// provider, a local failure - is terminal.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::CatalogError;
    ///
    /// let error = CatalogError::RateLimited { instance: "deezer".to_owned(), retry_after_ms: 500 };
    /// assert!(error.is_transient());
    /// assert!(!CatalogError::NotFound.is_transient());
    /// ```
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            CatalogError::Unavailable { .. } | CatalogError::RateLimited { .. }
        )
    }

    /// Returns the delay the instance asked the caller to wait, when it named
    /// one.
    ///
    /// # Returns
    ///
    /// The requested delay in milliseconds for [`CatalogError::RateLimited`],
    /// and `None` for every other variant.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::CatalogError;
    ///
    /// let error = CatalogError::RateLimited { instance: "deezer".to_owned(), retry_after_ms: 250 };
    /// assert_eq!(error.retry_after_ms(), Some(250));
    /// ```
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            CatalogError::RateLimited { retry_after_ms, .. } => Some(*retry_after_ms),
            _ => None,
        }
    }
}
