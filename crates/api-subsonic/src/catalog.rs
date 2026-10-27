//! The catalog settings and error mapping the entity-facing endpoints share.
//!
//! Every artist, album and track a response describes is a stored row, so this
//! module no longer resolves provider entities: what remains is the title
//! suffix a third-party client's copy of a remote entity carries
//! (`subsonic.catalog_title_suffix`), the mapping of a [`CatalogError`] onto the
//! failure envelope, and the reading of an entity's canonical identifiers,
//! which the `musicBrainzId` and `isrc` members are built from.
//!
//! An entity that is remote - stored, known to a provider, but without a file -
//! is still projected from its row: its container and MIME type are unknown
//! because no file exists yet, so the projection emits those legacy fields
//! empty, and the suffix tells a user which rows are not local.

use hearthsound_catalog::CatalogError;
use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::providers::ExternalId;

use crate::browse;
use crate::response::{SubsonicError, codes};

/// Reads the catalog title suffix from a settings snapshot.
///
/// # Parameters
///
/// - `snapshot`: the settings in effect.
///
/// # Returns
///
/// The suffix appended to a remote entity's title, empty by default.
///
/// # Errors
///
/// Returns a generic Subsonic error when the setting cannot be read.
pub(crate) fn title_suffix(snapshot: &SettingsStore) -> Result<String, SubsonicError> {
    snapshot
        .get(keys::SUBSONIC_CATALOG_TITLE_SUFFIX)
        .map_err(|error| browse::failure("subsonic.catalog_title_suffix", &error))
}

/// Maps a catalog failure onto the failure envelope.
///
/// # Parameters
///
/// - `error`: the failure a catalog call returned.
///
/// # Returns
///
/// Subsonic error 70 when the entity does not exist, and a generic error
/// carrying the provider's message otherwise, because neither an unreachable
/// instance nor a rejected credential is something a client can act on, while
/// an operator reading the log wants to know which instance failed.
pub(crate) fn failure(error: &CatalogError) -> SubsonicError {
    match error {
        CatalogError::NotFound => SubsonicError::not_found(),
        other => {
            tracing::warn!(error = %other, "a catalog call failed");
            SubsonicError::new(
                codes::GENERIC,
                format!("The catalog could not answer: {other}"),
            )
        }
    }
}

/// Returns the value an entity carries in one identifier namespace.
///
/// # Parameters
///
/// - `ids`: the entity's external identifiers.
/// - `namespace`: the namespace to read, for example `mb-release`.
///
/// # Returns
///
/// The value, or an empty string when the entity has none in that namespace.
pub(crate) fn identifier(ids: &[ExternalId], namespace: &str) -> String {
    ids.iter()
        .find(|id| id.namespace == namespace)
        .map(|id| id.value.clone())
        .unwrap_or_default()
}

/// Returns the values an entity carries in one identifier namespace.
///
/// A track may carry more than one ISRC, so the child's `isrc` member is a list.
///
/// # Parameters
///
/// - `ids`: the entity's external identifiers.
/// - `namespace`: the namespace to read, for example `isrc`.
///
/// # Returns
///
/// The values, in the order the identifiers were read.
pub(crate) fn identifiers(ids: &[ExternalId], namespace: &str) -> Vec<String> {
    ids.iter()
        .filter(|id| id.namespace == namespace)
        .map(|id| id.value.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an identifier.
    ///
    /// # Parameters
    ///
    /// - `namespace`: the identifier's namespace.
    /// - `value`: the identifier's value.
    fn id(namespace: &str, value: &str) -> ExternalId {
        ExternalId {
            namespace: namespace.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn a_known_namespace_yields_its_value() {
        let ids = [
            id("isrc", "USQX91300105"),
            id("mb-release", "release-1"),
            id("isrc", "GBAYE0000001"),
        ];

        assert_eq!(identifier(&ids, "mb-release"), "release-1");
        assert_eq!(
            identifiers(&ids, "isrc"),
            ["USQX91300105".to_owned(), "GBAYE0000001".to_owned()]
        );
    }

    #[test]
    fn a_missing_namespace_is_empty() {
        assert_eq!(identifier(&[], "mb-release"), "");
        assert!(identifiers(&[], "isrc").is_empty());
    }
}
