//! The catalog settings and error mapping the catalog-facing endpoints share.
//!
//! A catalog entity is one a provider knows but the server has not stored. When
//! a response describes one, its container and MIME type are unknown, because no
//! file exists yet: the projection emits those legacy fields empty, and a client
//! that trusts them cannot prepare for bytes the server has not produced. The
//! suffix appended to every catalog title (`subsonic.catalog_title_suffix`) does
//! come from a setting, and tells a user which results are not local. It is read
//! here so `search3` and the catalog-id browse endpoints agree on it.
//!
//! The module also names the artists a catalog entity credits: a provider
//! record carries an artist's identifiers as external ids, and the first one
//! outside the canonical namespaces is what the issuing instance resolves. And
//! it maps a [`CatalogError`] onto the failure envelope: an entity that does not
//! exist is a Subsonic not-found, while anything else is a failure the client
//! cannot act on and is reported as a generic error carrying the provider's
//! message for an operator.

use hearthsound_catalog::CatalogError;
use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::providers::{self, ArtistCredit};
use hearthsound_core::{CatalogId, EntityKind, Slug};

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
/// The suffix appended to a catalog entity's title, empty by default.
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

/// Builds the catalog id of the artist one credit names.
///
/// A provider record carries an artist's credit as a name plus the external
/// identifiers it knows for that artist. A catalog provider puts its own id for
/// the artist in that list, under a namespace that is not canonical, and that
/// id is what the issuing instance resolves, so it becomes the payload of an
/// `x_<slug>_r_<payload>` id. A credit that carries only canonical identifiers
/// yields `None`, and the caller emits an empty `artistId` rather than an id the
/// catalog cannot resolve.
///
/// # Parameters
///
/// - `slug`: the slug of the instance that issued the entity.
/// - `credit`: the artist credit.
///
/// # Returns
///
/// The artist's catalog id, or `None` when the credit carries no provider
/// identifier.
pub(crate) fn artist_id(slug: &Slug, credit: &ArtistCredit) -> Option<CatalogId> {
    let id = providers::credit_provider_id(credit)?;
    CatalogId::new(slug.clone(), EntityKind::Artist, id.value.clone()).ok()
}

/// Returns the MusicBrainz identifier an entity's external ids carry.
///
/// # Parameters
///
/// - `ids`: the entity's external identifiers.
///
/// # Returns
///
/// The value namespaced `musicbrainz`, or an empty string when there is none.
pub(crate) fn music_brainz_id(ids: &[hearthsound_core::providers::ExternalId]) -> String {
    ids.iter()
        .find(|id| id.namespace == "musicbrainz")
        .map(|id| id.value.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_core::providers::ExternalId;

    /// Builds a credit carrying the given identifiers.
    ///
    /// # Parameters
    ///
    /// - `ids`: the namespace and value of each identifier, in credit order.
    fn credit(ids: &[(&str, &str)]) -> ArtistCredit {
        ArtistCredit {
            name: "Daft Punk".to_owned(),
            ids: ids
                .iter()
                .map(|(namespace, value)| ExternalId {
                    namespace: (*namespace).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
        }
    }

    /// Returns the slug of the instance the tests resolve ids for.
    fn slug() -> Slug {
        Slug::new("deezer").unwrap()
    }

    #[test]
    fn an_artist_id_is_built_from_the_credits_provider_id() {
        let credit = credit(&[("mb-artist", "artist-1"), ("deezer", "27")]);

        let id = artist_id(&slug(), &credit).unwrap();

        assert_eq!(id.slug(), &slug());
        assert_eq!(id.kind(), EntityKind::Artist);
        assert_eq!(id.provider_id(), "27");
    }

    #[test]
    fn a_credit_of_canonical_identifiers_names_no_catalog_artist() {
        let canonical = credit(&[("mb-artist", "artist-1"), ("isrc", "USQX91300105")]);

        assert_eq!(artist_id(&slug(), &canonical), None);
        assert_eq!(artist_id(&slug(), &credit(&[])), None);
    }
}
