//! Identity resolution through the metadata layer.
//!
//! A track can be acquired, streamed and retagged without ever knowing which
//! canonical recording it is. When the server wants to know - to improve a
//! search, to write MusicBrainz tags at import, to join two providers that name
//! the same recording - it asks the metadata layer, which is what this module
//! does.
//!
//! The layer policy (02-architecture.md, section 5) is *ordered, first match
//! with score >= 0.9 wins else best score across instances*: the instances are
//! tried in the administrator's order, the first one whose best recording
//! reaches [`IDENTITY_MATCH_THRESHOLD`] decides, and when no instance is that
//! confident the highest score any of them reported decides. Two instances'
//! recordings are never merged into one answer.
//!
//! Resolution is best effort and never blocks a play: the acquisition document
//! (`06-acquisition-and-playback.md`, section 3) has the orchestrator proceed
//! with whatever identity is known and let a resolution that finishes later
//! upgrade the rows. Every answer is therefore cached in `catalog_cache`, keyed
//! by the metadata instance and the request, so a repeated request for one track
//! does not ask MusicBrainz again.
//!
//! What a resolution is applied to is the track it names: this module writes the
//! recording's MusicBrainz recording id to the track and records the
//! `mb-recording`, `mb-release`, `mb-release-group` and `mb-artist` aliases of
//! the canonical identifiers it learned, in one short transaction. Two providers
//! then name one row: the catalog provider's `p:<slug>` id from materialization
//! and the MusicBrainz ids from resolution.

use hearthsound_core::providers::{RecordingHint, RecordingMatch};
use hearthsound_core::{EntityId, EntityKind, Layer, Slug};
use hearthsound_library::tracks;
use hearthsound_library::{ArtistInput, Origin, artists};

use crate::cache::{ENTITY_CACHE_TTL, RequestKey};
use crate::dedupe::normalize_isrc;
use crate::error::CatalogError;
use crate::materialize::{
    MB_ARTIST, MB_RECORDING, MB_RELEASE, MB_RELEASE_GROUP, canonical_id, insert_alias,
};
use crate::service::CatalogService;
use crate::wire::WireRecordingMatches;

/// The confidence at which a metadata instance's answer is taken as final.
///
/// Point nine. Below it the answer is still usable, but a later instance may
/// report a better one, which is why the policy is "first instance that reaches
/// the threshold, otherwise the best score".
pub const IDENTITY_MATCH_THRESHOLD: f32 = 0.9;

/// What identity resolution asks a metadata instance about.
///
/// An ISRC is the strongest identifier a caller can hold, and a hint is
/// everything else: the title, the artists, the album and the duration the
/// caller already knows. The two are separate requests because they are
/// separate questions with separate cache entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityRequest {
    /// The recording's International Standard Recording Code.
    Isrc(String),
    /// A hint built from what the caller knows about the recording.
    Hint(RecordingHint),
}

impl IdentityRequest {
    /// Builds a request from an ISRC, when there is one.
    ///
    /// # Parameters
    ///
    /// - `isrc`: the ISRC a track carries, when it has one.
    ///
    /// # Returns
    ///
    /// An ISRC request, or `None` when there is no usable ISRC.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::IdentityRequest;
    ///
    /// assert!(IdentityRequest::from_isrc(None).is_none());
    /// assert!(IdentityRequest::from_isrc(Some("  ")).is_none());
    /// assert_eq!(
    ///     IdentityRequest::from_isrc(Some(" usrc17607839 ")),
    ///     Some(IdentityRequest::Isrc("USRC17607839".to_owned()))
    /// );
    /// ```
    pub fn from_isrc(isrc: Option<&str>) -> Option<IdentityRequest> {
        normalize_isrc(isrc).map(IdentityRequest::Isrc)
    }

    /// Returns the key this request is cached under.
    pub(crate) fn cache_key(&self) -> RequestKey {
        match self {
            IdentityRequest::Isrc(isrc) => RequestKey::recording_isrc(isrc),
            IdentityRequest::Hint(hint) => RequestKey::recording_hint(hint),
        }
    }

    /// Builds the hint a metadata instance is asked with.
    pub(crate) fn hint(&self) -> RecordingHint {
        match self {
            IdentityRequest::Isrc(isrc) => RecordingHint {
                title: String::new(),
                artists: Vec::new(),
                album: None,
                duration_ms: None,
                isrc: Some(isrc.clone()),
            },
            IdentityRequest::Hint(hint) => hint.clone(),
        }
    }
}

/// A recording a metadata instance matched, with the instance that matched it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRecording {
    /// The metadata instance that answered.
    pub instance: Slug,
    /// The recording it matched.
    pub recording: RecordingMatch,
}

impl CatalogService {
    /// Resolves a recording through the metadata layer.
    ///
    /// The instances are tried in the administrator's order until one of them
    /// matches at [`IDENTITY_MATCH_THRESHOLD`] or better; when none does, the
    /// highest score any of them reported is the answer. Every instance's answer
    /// is cached for [`ENTITY_CACHE_TTL`] under its own id and the request, so a
    /// repeated resolution does not spend the provider's rate limit again.
    ///
    /// # Parameters
    ///
    /// - `request`: the ISRC or the hint to resolve.
    ///
    /// # Returns
    ///
    /// The winning recording and the instance that reported it, or `None` when
    /// no instance knows the recording.
    ///
    /// # Errors
    ///
    /// Returns the mapped failure of the last instance that failed, and an empty
    /// layer is not an error: it is `None`.
    pub async fn resolve_recording(
        &self,
        request: &IdentityRequest,
    ) -> Result<Option<ResolvedRecording>, CatalogError> {
        let key = request.cache_key();
        let hint = request.hint();
        let mut best: Option<ResolvedRecording> = None;
        let mut failure: Option<CatalogError> = None;

        for instance in self.registry().layers(Layer::Metadata) {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            let outcome = self
                .cache()
                .through::<Vec<RecordingMatch>, WireRecordingMatches, _>(
                    instance.id(),
                    &key,
                    ENTITY_CACHE_TTL,
                    async {
                        let outcome = provider.resolve_recording(&hint).await;
                        self.finish(instance.slug(), "resolve-recording", Ok(outcome))
                    },
                )
                .await;
            let matches = match outcome {
                Ok(matches) => matches,
                Err(error) => {
                    failure = Some(error);
                    continue;
                }
            };
            let Some(top) = best_match(matches) else {
                continue;
            };
            if top.score >= IDENTITY_MATCH_THRESHOLD {
                return Ok(Some(ResolvedRecording {
                    instance: instance.slug().clone(),
                    recording: top,
                }));
            }
            if best
                .as_ref()
                .is_none_or(|current| top.score > current.recording.score)
            {
                best = Some(ResolvedRecording {
                    instance: instance.slug().clone(),
                    recording: top,
                });
            }
        }

        match (best, failure) {
            (Some(resolved), _) => Ok(Some(resolved)),
            (None, Some(error)) => Err(error),
            (None, None) => Ok(None),
        }
    }

    /// Resolves a recording and applies what was learned to a stored track.
    ///
    /// The recording's MusicBrainz id is written to the track, and the canonical
    /// identifiers the match carries are recorded as aliases: `mb-recording` on
    /// the track, `mb-release` and `mb-release-group` on the album it belongs
    /// to, and `mb-artist` on the artist rows the match names. All of it happens
    /// in one short transaction, so a track never holds half of a resolved
    /// identity.
    ///
    /// The call is what an acquisition orchestrator awaits or spawns: a `play`
    /// acquisition proceeds with whatever identity is known and lets this finish
    /// later.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the stored track to enrich.
    /// - `request`: the ISRC or the hint to resolve.
    ///
    /// # Returns
    ///
    /// The recording that was applied, or `None` when no instance knows the
    /// recording, in which case the track is left untouched.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the track does not exist,
    /// [`CatalogError::Database`] or [`CatalogError::Library`] when a row cannot
    /// be written, and the mapped failure of the metadata layer.
    pub async fn resolve_track_identity(
        &self,
        track_id: EntityId,
        request: &IdentityRequest,
    ) -> Result<Option<ResolvedRecording>, CatalogError> {
        let Some(resolved) = self.resolve_recording(request).await? else {
            return Ok(None);
        };
        self.apply_recording(track_id, &resolved.recording).await?;
        Ok(Some(resolved))
    }

    /// Writes one resolved recording onto a stored track.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the stored track.
    /// - `recording`: the recording a metadata instance matched.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the track does not exist,
    /// [`CatalogError::Database`] or [`CatalogError::Library`] when a row cannot
    /// be written, and [`CatalogError::CorruptAlias`] when a stored id does not
    /// decode.
    async fn apply_recording(
        &self,
        track_id: EntityId,
        recording: &RecordingMatch,
    ) -> Result<(), CatalogError> {
        let mut tx = self.database().writer().begin().await?;
        let Some(track) = tracks::get_track(&mut tx, track_id).await? else {
            return Err(CatalogError::NotFound);
        };

        if let Some(mbid) = canonical_id(&recording.ids, MB_RECORDING) {
            if track.mb_recording_id.as_deref() != Some(mbid) {
                tracks::set_track_mb_recording_id(&mut tx, track_id, mbid).await?;
            }
            insert_alias(&mut tx, MB_RECORDING, EntityKind::Track, mbid, track_id).await?;
        }

        if let Some(album) = track.album_id {
            for release in &recording.releases {
                for id in &release.ids {
                    if id.value.is_empty()
                        || (id.namespace != MB_RELEASE && id.namespace != MB_RELEASE_GROUP)
                    {
                        continue;
                    }
                    insert_alias(&mut tx, &id.namespace, EntityKind::Album, &id.value, album)
                        .await?;
                }
            }
        } else if recording
            .releases
            .iter()
            .any(|release| !release.ids.is_empty())
        {
            tracing::debug!(
                track = %track_id,
                "a resolved release cannot be aliased to a track that belongs to no album"
            );
        }

        for credit in &recording.artists {
            let Some(mbid) = canonical_id(&credit.ids, MB_ARTIST) else {
                continue;
            };
            let input = ArtistInput::new(credit.name.as_str())
                .mb_artist_id(mbid)
                .origin(Origin::Catalog);
            let artist = artists::upsert_artist(&mut tx, &input).await?;
            insert_alias(&mut tx, MB_ARTIST, EntityKind::Artist, mbid, artist).await?;
        }

        tx.commit().await?;
        Ok(())
    }
}

/// Returns the recording a metadata instance is most confident about.
///
/// A provider answers best first, so the first of two equally scored recordings
/// is the one to keep.
///
/// # Parameters
///
/// - `matches`: the recordings an instance reported.
///
/// # Returns
///
/// The highest scoring recording, or `None` when the instance knows nothing.
fn best_match(matches: Vec<RecordingMatch>) -> Option<RecordingMatch> {
    let mut best: Option<RecordingMatch> = None;
    for candidate in matches {
        let better = match &best {
            Some(current) => candidate.score > current.score,
            None => true,
        };
        if better {
            best = Some(candidate);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_core::providers::{ArtistCredit, ExternalId, ReleaseSummary};

    /// Builds a match for the ranking tests.
    fn matched(score: f32) -> RecordingMatch {
        RecordingMatch {
            ids: vec![ExternalId {
                namespace: MB_RECORDING.to_owned(),
                value: "rec-1".to_owned(),
            }],
            title: "Cafe del Mar".to_owned(),
            artists: vec![ArtistCredit {
                name: "Energy 52".to_owned(),
                ids: vec![],
            }],
            duration_ms: None,
            releases: vec![ReleaseSummary {
                ids: vec![],
                title: "Cafe del Mar".to_owned(),
                date: None,
                track_count: None,
            }],
            score,
        }
    }

    #[test]
    fn the_confident_recording_wins() {
        let best = best_match(vec![matched(0.4), matched(0.95)]).unwrap();

        assert_eq!(best.score, 0.95);
    }

    #[test]
    fn the_first_of_two_equal_scores_wins() {
        let mut first = matched(0.8);
        first.title = "first".to_owned();
        let mut second = matched(0.8);
        second.title = "second".to_owned();

        let best = best_match(vec![first, second]).unwrap();

        assert_eq!(best.title, "first");
    }

    #[test]
    fn an_instance_that_knows_nothing_ranks_nothing() {
        assert!(best_match(Vec::new()).is_none());
    }

    #[test]
    fn an_isrc_request_is_normalized_and_a_blank_one_is_dropped() {
        assert_eq!(
            IdentityRequest::from_isrc(Some("usrc17607839")),
            Some(IdentityRequest::Isrc("USRC17607839".to_owned()))
        );
        assert_eq!(IdentityRequest::from_isrc(Some("")), None);
        assert_eq!(IdentityRequest::from_isrc(None), None);
    }

    #[test]
    fn an_isrc_request_becomes_a_hint_that_carries_only_the_isrc() {
        let hint = IdentityRequest::Isrc("USRC17607839".to_owned()).hint();

        assert_eq!(hint.isrc.as_deref(), Some("USRC17607839"));
        assert!(hint.title.is_empty());
        assert!(hint.artists.is_empty());
        assert_eq!(hint.album, None);
    }

    #[test]
    fn the_two_request_kinds_are_cached_separately() {
        let hint = RecordingHint {
            title: "Cafe del Mar".to_owned(),
            artists: vec!["Energy 52".to_owned()],
            album: None,
            duration_ms: Some(180_000),
            isrc: Some("USRC17607839".to_owned()),
        };

        assert_ne!(
            IdentityRequest::Hint(hint).cache_key(),
            IdentityRequest::Isrc("USRC17607839".to_owned()).cache_key()
        );
    }
}
