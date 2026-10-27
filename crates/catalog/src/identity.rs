//! Recording identity: which recording a file is, according to the metadata layer.
//!
//! A scanned file carries whatever tags its writer left. When those tags name no
//! recording, the acquisition path asks the metadata layer which recording the
//! file is, and the answer is what the stored row is identified by. This module
//! is that question and its answer; writing the identifiers onto the rows is the
//! caller's transaction, because only the caller knows which row it means.
//!
//! Instances are tried in the administrator's order until one matches at
//! [`IDENTITY_MATCH_THRESHOLD`] or better. When none does, the highest score any
//! of them reported is the answer: a weak match is still better evidence than
//! none, and the caller decides whether to apply it.

use std::sync::Arc;

use hearthsound_core::providers::{RecordingHint, RecordingMatch};
use hearthsound_core::{Layer, Slug};
use hearthsound_plugin_host::ProviderRegistry;

use crate::error::CatalogError;

/// The score a match needs to be applied without asking another instance.
pub const IDENTITY_MATCH_THRESHOLD: f32 = 0.85;

/// What the caller knows about the recording it is looking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityRequest {
    /// An International Standard Recording Code, which names one recording.
    Isrc(String),
    /// The tags a file carries, which name a recording only loosely.
    Hint(RecordingHint),
}

impl IdentityRequest {
    /// Builds a request from an ISRC, when there is a usable one.
    ///
    /// # Parameters
    ///
    /// - `isrc`: the ISRC a file or a row carries.
    ///
    /// # Returns
    ///
    /// The request, or `None` when the ISRC is absent or blank.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::IdentityRequest;
    ///
    /// assert!(IdentityRequest::from_isrc(Some("USRC17607839")).is_some());
    /// assert!(IdentityRequest::from_isrc(Some("  ")).is_none());
    /// assert!(IdentityRequest::from_isrc(None).is_none());
    /// ```
    pub fn from_isrc(isrc: Option<&str>) -> Option<IdentityRequest> {
        let isrc = isrc?.trim();
        (!isrc.is_empty()).then(|| IdentityRequest::Isrc(isrc.to_uppercase()))
    }

    /// Returns the hint the metadata layer is asked with.
    pub fn hint(&self) -> RecordingHint {
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

    /// Returns a stable key for the request, for a cache.
    pub fn cache_key(&self) -> String {
        match self {
            IdentityRequest::Isrc(isrc) => format!("isrc:{isrc}"),
            IdentityRequest::Hint(hint) => format!(
                "hint:{}|{}|{}|{:?}",
                hint.title,
                hint.artists.join(","),
                hint.album.clone().unwrap_or_default(),
                hint.duration_ms
            ),
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

/// Resolves recording identities through the metadata layer.
pub struct IdentityService {
    registry: Arc<ProviderRegistry>,
}

impl IdentityService {
    /// Builds the service over the provider registry.
    ///
    /// # Parameters
    ///
    /// - `registry`: the provider registry.
    pub fn new(registry: Arc<ProviderRegistry>) -> IdentityService {
        IdentityService { registry }
    }

    /// Resolves a recording through the metadata layer.
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
    /// Returns [`CatalogError::NotFound`] when the metadata layer serves no
    /// instance and [`CatalogError`] mapped from the last failure otherwise.
    pub async fn resolve_recording(
        &self,
        request: &IdentityRequest,
    ) -> Result<Option<ResolvedRecording>, CatalogError> {
        let hint = request.hint();
        let mut best: Option<ResolvedRecording> = None;
        let mut failure: Option<CatalogError> = None;
        for instance in self.registry.layers(Layer::Metadata) {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            let matches = match provider.resolve_recording(&hint).await {
                Ok(matches) => matches,
                Err(error) => {
                    failure = Some(CatalogError::from_provider(instance.slug().as_str(), error));
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
}

/// Returns the highest-scoring match, if there is one.
///
/// # Parameters
///
/// - `matches`: the matches an instance reported.
fn best_match(mut matches: Vec<RecordingMatch>) -> Option<RecordingMatch> {
    matches.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    matches.into_iter().next()
}
