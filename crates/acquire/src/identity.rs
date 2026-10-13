//! Resolving a wanted track's recording identity before its search runs.
//!
//! This is the first stage of the pipeline. It asks the metadata layer to turn
//! what the stored track knows - its ISRC, or a hint built from its title,
//! artists, album and duration - into canonical recording identifiers, which the
//! metadata plugin writes onto the track and its album and artists through
//! [`CatalogService::resolve_track_identity`]. The improved tags are what the
//! import stage writes into the file.
//!
//! The stage is best-effort: a metadata instance that is unreachable, rate
//! limited or simply does not know the recording leaves the track as it is, and
//! the search runs with the identity the track already had. An acquisition never
//! fails because identity resolution did.
//!
//! A `play` acquisition does not wait for the network round trip. Its resolution
//! is spawned and the pipeline moves on to the search at once; whatever the task
//! learns is applied to the rows when it finishes, so a client waiting for audio
//! never waits for MusicBrainz. Every other priority awaits the resolution,
//! bounded by the call deadline of the metadata instances, so a bulk import gets
//! the best tags without an unbounded wait.

use std::sync::Arc;
use std::time::Duration;

use hearthsound_catalog::IdentityRequest;
use hearthsound_core::Layer;
use hearthsound_core::Priority;
use hearthsound_core::providers::RecordingHint;
use hearthsound_plugin_host::ProviderRegistry;

use crate::orchestrator::AcquisitionOrchestrator;
use crate::repo::AcquisitionRow;

/// The bound on an awaited identity resolution when no metadata instance
/// advertises a call timeout of its own, in milliseconds.
///
/// It mirrors the plugin host's default `resources.callTimeoutMs`, so a server
/// with an empty metadata layer does not wait longer than a single plugin call
/// would be allowed to.
const IDENTITY_DEADLINE_FALLBACK_MS: u32 = 30_000;

impl AcquisitionOrchestrator {
    /// Resolves and applies the recording identity of an acquisition's track.
    ///
    /// The identity the track already has is left alone: a track that carries a
    /// MusicBrainz recording id is complete, so nothing is resolved. Otherwise
    /// the resolution is requested by ISRC when the track has one and by a hint
    /// built from its title, artists, album and duration when it does not.
    ///
    /// A `play` acquisition spawns the resolution and returns; every other
    /// priority awaits it, bounded by [`identity_deadline`]. Nothing here is
    /// fallible: a metadata layer that cannot be reached is logged and the
    /// acquisition continues without the identity.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row, which names the track.
    pub(crate) async fn resolve_identity(&self, row: &AcquisitionRow) {
        let Some(track_id) = row.track_id else {
            return;
        };
        let track = match self.library().get_track(track_id).await {
            Ok(Some(track)) => track,
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(track = %track_id, %error, "a track could not be read for identity resolution");
                return;
            }
        };
        if track.mb_recording_id.is_some() {
            tracing::debug!(
                track = %track_id,
                "the track already carries a MusicBrainz recording id"
            );
            return;
        }
        let request = match IdentityRequest::from_isrc(track.isrc.as_deref()) {
            Some(request) => request,
            None => IdentityRequest::Hint(RecordingHint {
                title: track.title.clone(),
                artists: self.track_artists(track_id).await,
                album: self.track_album_title(track.album_id).await,
                duration_ms: track.duration_ms.and_then(|ms| u32::try_from(ms).ok()),
                isrc: track.isrc.clone(),
            }),
        };

        if row.priority == Priority::Play {
            let catalog = Arc::clone(self.catalog());
            tokio::spawn(async move {
                match catalog.resolve_track_identity(track_id, &request).await {
                    Ok(Some(resolved)) => tracing::info!(
                        track = %track_id,
                        instance = resolved.instance.as_str(),
                        "resolved a recording identity after the play request",
                    ),
                    Ok(None) => {}
                    Err(error) => tracing::debug!(
                        track = %track_id,
                        %error,
                        "resolving a recording identity failed"
                    ),
                }
            });
            return;
        }

        let deadline = identity_deadline(self.registry());
        match tokio::time::timeout(
            deadline,
            self.catalog().resolve_track_identity(track_id, &request),
        )
        .await
        {
            Ok(Ok(Some(resolved))) => tracing::info!(
                track = %track_id,
                instance = resolved.instance.as_str(),
                "resolved a recording identity",
            ),
            Ok(Ok(None)) => {}
            Ok(Err(error)) => tracing::warn!(
                track = %track_id,
                %error,
                "resolving a recording identity failed"
            ),
            Err(_) => tracing::warn!(
                track = %track_id,
                "resolving a recording identity timed out"
            ),
        }
    }

    /// Returns the names of a track's credited artists.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track to read.
    ///
    /// # Returns
    ///
    /// The artist names, or an empty list when the read failed.
    async fn track_artists(&self, track_id: hearthsound_core::EntityId) -> Vec<String> {
        match self.library().list_track_artists(track_id).await {
            Ok(artists) => artists.into_iter().map(|artist| artist.name).collect(),
            Err(error) => {
                tracing::debug!(track = %track_id, %error, "a track's artists could not be read");
                Vec::new()
            }
        }
    }

    /// Returns the title of the album a track belongs to, when it has one.
    ///
    /// # Parameters
    ///
    /// - `album_id`: the track's album id, when it belongs to an album.
    ///
    /// # Returns
    ///
    /// The album title, or `None` when the track has no album or the read
    /// failed.
    async fn track_album_title(
        &self,
        album_id: Option<hearthsound_core::EntityId>,
    ) -> Option<String> {
        let album_id = album_id?;
        match self.library().get_album(album_id).await {
            Ok(Some(album)) => Some(album.title),
            Ok(None) => None,
            Err(error) => {
                tracing::debug!(album = %album_id, %error, "an album could not be read");
                None
            }
        }
    }
}

/// Returns the bound on an awaited identity resolution.
///
/// # Parameters
///
/// - `registry`: the provider registry the metadata layer is read from.
///
/// # Returns
///
/// The longest `resources.callTimeoutMs` any metadata instance advertises, or
/// [`IDENTITY_DEADLINE_FALLBACK_MS`] when the layer is empty.
fn identity_deadline(registry: &ProviderRegistry) -> Duration {
    let millis = registry
        .layers(Layer::Metadata)
        .iter()
        .map(|instance| instance.manifest().resources.call_timeout_ms)
        .max()
        .unwrap_or(IDENTITY_DEADLINE_FALLBACK_MS);
    Duration::from_millis(u64::from(millis))
}
