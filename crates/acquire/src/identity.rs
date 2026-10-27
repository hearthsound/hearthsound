//! Resolving a wanted track's recording identity before its search runs.
//!
//! This is the first stage of the pipeline. It asks the metadata layer to turn
//! what the stored track knows - its ISRC, or a hint built from its title,
//! artists, album and duration - into canonical recording identifiers, and
//! writes them onto the track through
//! [`external_ids::attach`](hearthsound_library::external_ids::attach). A
//! recording another row already claims is merged into that row, so the
//! identifier never names two tracks. The improved identifiers are what the
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

use hearthsound_catalog::{CatalogError, IdentityRequest, IdentityService, ResolvedRecording};
use hearthsound_core::Layer;
use hearthsound_core::Priority;
use hearthsound_core::providers::{
    ExternalId, ISRC, MB_ARTIST, MB_RECORDING, MB_RELEASE, MB_RELEASE_GROUP, RecordingHint,
    RecordingMatch,
};
use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use hearthsound_library::{ArtistInput, albums, artists, entities, external_ids, sort_key, tracks};
use hearthsound_plugin_host::ProviderRegistry;

use crate::error::AcquireError;
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
    /// built from its title, artists, album and duration when it does not, and
    /// the identifiers the metadata layer reports are attached with
    /// [`external_ids::attach`]. An identifier another track row already claims
    /// merges the two rows, and the acquisition continues on the survivor.
    ///
    /// A `play` acquisition spawns the resolution and returns; every other
    /// priority awaits it, bounded by [`identity_deadline`]. Nothing here is
    /// fallible: a metadata layer that cannot be reached is logged and the
    /// acquisition continues without the identity.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row, which names the track. Its
    ///   `track_id` is re-pointed when a merge moved the track.
    pub(crate) async fn resolve_identity(&self, row: &mut AcquisitionRow) {
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
        let stored = match self.stored_ids(EntityKind::Track, track_id).await {
            Ok(ids) => ids,
            Err(error) => {
                tracing::debug!(track = %track_id, %error, "a track's identifiers could not be read");
                return;
            }
        };
        if stored.iter().any(|id| id.namespace == MB_RECORDING) {
            tracing::debug!(
                track = %track_id,
                "the track already carries a MusicBrainz recording id"
            );
            return;
        }
        let isrc = stored
            .iter()
            .find(|id| id.namespace == ISRC)
            .map(|id| id.value.clone());
        let request = match IdentityRequest::from_isrc(isrc.as_deref()) {
            Some(request) => request,
            None => IdentityRequest::Hint(RecordingHint {
                title: track.title.clone(),
                artists: self.track_artists(track_id).await,
                album: self.track_album_title(track.album_id).await,
                duration_ms: track.duration_ms.and_then(|ms| u32::try_from(ms).ok()),
                isrc,
            }),
        };

        if row.priority == Priority::Play {
            // The identifiers are written asynchronously, so the acquisition's
            // own stages never wait for them; a merge this task performs after
            // the pipeline read its row is picked up from the ledger by the
            // next attempt.
            let orchestrator = self.clone();
            tokio::spawn(async move {
                match orchestrator.resolve_recording(&request).await {
                    Ok(Some(resolved)) => {
                        orchestrator
                            .apply_recording(track_id, &resolved.recording)
                            .await;
                        tracing::info!(
                            track = %track_id,
                            instance = resolved.instance.as_str(),
                            "resolved a recording identity after the play request",
                        );
                    }
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
        match tokio::time::timeout(deadline, self.resolve_recording(&request)).await {
            Ok(Ok(Some(resolved))) => {
                tracing::info!(
                    track = %track_id,
                    instance = resolved.instance.as_str(),
                    "resolved a recording identity",
                );
                let survivor = self.apply_recording(track_id, &resolved.recording).await;
                row.track_id = Some(survivor);
            }
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

    /// Asks the metadata layer to name a recording.
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
    /// Returns [`CatalogError`] when every instance that could answer failed.
    async fn resolve_recording(
        &self,
        request: &IdentityRequest,
    ) -> Result<Option<ResolvedRecording>, CatalogError> {
        let identity = IdentityService::new(Arc::clone(self.catalog().registry()));
        identity.resolve_recording(request).await
    }

    /// Writes a resolved recording onto its track, album and artists.
    ///
    /// The recording's own identifiers name the track, the release the match
    /// offers identifies the album the track belongs to, and the recording's
    /// credited acts identify the artists the track credits. An identifier
    /// another row already owns merges the two rows, so one identity never names
    /// two rows; the merge moves the ledger's reference along with the loser's
    /// rows, and the caller re-points its in-memory row at the survivor.
    ///
    /// Everything happens in one transaction, so a track never holds half of a
    /// resolved identity.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track the identifiers belong to.
    /// - `recording`: the recording a metadata instance matched.
    ///
    /// # Returns
    ///
    /// The track row that stands for the recording afterwards: the same row, or
    /// the survivor of a merge.
    async fn apply_recording(&self, track_id: EntityId, recording: &RecordingMatch) -> EntityId {
        let mut transaction = match self.database().writer().begin().await {
            Ok(transaction) => transaction,
            Err(error) => {
                tracing::warn!(
                    track = %track_id,
                    %error,
                    "a resolved recording's identity could not be written"
                );
                return track_id;
            }
        };
        let survivor = match write_recording(&mut transaction, track_id, recording).await {
            Ok(survivor) => survivor,
            Err(error) => {
                tracing::warn!(
                    track = %track_id,
                    %error,
                    "a resolved recording's identity could not be written"
                );
                return track_id;
            }
        };
        if let Err(error) = transaction.commit().await {
            tracing::warn!(
                track = %track_id,
                %error,
                "a resolved recording's identity could not be committed"
            );
            return track_id;
        }
        survivor
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

/// Writes one resolved recording onto the rows that describe it.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `track_id`: the track the recording names.
/// - `recording`: the recording a metadata instance matched.
///
/// # Returns
///
/// The track row that stands for the recording afterwards.
///
/// # Errors
///
/// Returns the library or database error that stopped the write.
async fn write_recording(
    conn: &mut sqlx::SqliteConnection,
    track_id: EntityId,
    recording: &RecordingMatch,
) -> Result<EntityId, AcquireError> {
    let track = attach_and_merge(conn, EntityKind::Track, track_id, &recording.ids).await?;
    let Some(stored) = tracks::get_track(conn, track).await? else {
        return Ok(track);
    };
    if let Some(album) = stored.album_id {
        identify_album(conn, album, recording).await?;
    }
    identify_credited_artists(conn, track, recording).await?;
    Ok(track)
}

/// Attaches every identifier to one row and merges the rows that already own one.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `kind`: the entity kind.
/// - `row`: the row the identifiers belong to.
/// - `ids`: the identifiers to attach.
///
/// # Returns
///
/// The row that stands for the entity afterwards: the same row, or the survivor
/// of a merge.
///
/// # Errors
///
/// Returns the library error of the attach or the merge.
async fn attach_and_merge(
    conn: &mut sqlx::SqliteConnection,
    kind: EntityKind,
    row: EntityId,
    ids: &[ExternalId],
) -> Result<EntityId, AcquireError> {
    let mut current = row;
    let conflicts = external_ids::attach(conn, kind, current.uuid(), ids).await?;
    for conflict in conflicts {
        let survivor = entities::merge(conn, kind, current.uuid(), conflict.owner).await?;
        let survivor = EntityId::from_uuid(IdPrefix::from_entity_kind(kind), survivor);
        if survivor != current {
            tracing::info!(
                entity = %current,
                survivor = %survivor,
                namespace = conflict.namespace.as_str(),
                value = conflict.value.as_str(),
                "merged a row into the one that already claimed its identifier"
            );
        }
        current = survivor;
    }
    Ok(current)
}

/// Identifies the album a track belongs to with the release a recording names.
///
/// A recording appears on several releases - an album, its reissue, a
/// compilation - and the row the track belongs to is one of them. The release to
/// identify it with is the one the match offers when it offers a single one, and
/// otherwise the release whose title is the album's, because that is the release
/// the album row was built from.
///
/// # Parameters
///
/// - `conn`: the transaction to read and write on.
/// - `album`: the album row the track belongs to.
/// - `recording`: the recording a metadata instance matched.
///
/// # Errors
///
/// Returns the library error of a read, an attach or a merge.
async fn identify_album(
    conn: &mut sqlx::SqliteConnection,
    album: EntityId,
    recording: &RecordingMatch,
) -> Result<(), AcquireError> {
    let mut releases: Vec<(&str, &str, Option<&str>)> = Vec::new();
    for release in &recording.releases {
        let Some(id) = canonical_id(&release.ids, MB_RELEASE) else {
            continue;
        };
        if releases.iter().any(|(known, _, _)| *known == id) {
            continue;
        }
        releases.push((
            id,
            release.title.as_str(),
            canonical_id(&release.ids, MB_RELEASE_GROUP),
        ));
    }
    let chosen = match releases.as_slice() {
        [] => None,
        [only] => Some(*only),
        several => {
            let title = albums::get_album(conn, album)
                .await?
                .map(|stored| sort_key(&stored.title))
                .unwrap_or_default();
            several
                .iter()
                .find(|(_, release_title, _)| sort_key(release_title) == title)
                .copied()
        }
    };
    let stored = match chosen {
        Some((release, _, group)) => albums::identify_album(conn, album, release, group).await?,
        None => album,
    };
    let ids: Vec<ExternalId> = recording
        .releases
        .iter()
        .flat_map(|release| release.ids.iter())
        .filter(|id| id.namespace == MB_RELEASE || id.namespace == MB_RELEASE_GROUP)
        .filter(|id| !id.value.is_empty())
        .cloned()
        .collect();
    attach_and_merge(conn, EntityKind::Album, stored, &ids).await?;
    Ok(())
}

/// Identifies the artists a track credits with the ones a recording names.
///
/// A resolution names the act by its MusicBrainz artist id and by its name, and
/// the track names the same act by the row it credits. The two are matched by
/// name, and the row the track credits is the one identified: a resolution
/// therefore never creates a second row for an act the track already holds.
///
/// # Parameters
///
/// - `conn`: the transaction to read and write on.
/// - `track_id`: the track the recording was applied to.
/// - `recording`: the recording a metadata instance matched.
///
/// # Errors
///
/// Returns the library error of a read, an attach, a merge or a write.
async fn identify_credited_artists(
    conn: &mut sqlx::SqliteConnection,
    track_id: EntityId,
    recording: &RecordingMatch,
) -> Result<(), AcquireError> {
    let credited = artists::list_track_artists(conn, track_id).await?;
    let mut taken: Vec<EntityId> = Vec::with_capacity(credited.len());
    for credit in &recording.artists {
        let Some(mbid) = canonical_id(&credit.ids, MB_ARTIST) else {
            continue;
        };
        let name = sort_key(&credit.name);
        let resolved = match credited
            .iter()
            .find(|artist| !taken.contains(&artist.id) && sort_key(&artist.name) == name)
        {
            Some(artist) => artists::identify_artist(conn, artist.id, mbid).await?,
            None => {
                let mut input = ArtistInput::new(credit.name.as_str());
                input.ids = vec![ExternalId {
                    namespace: MB_ARTIST.to_owned(),
                    value: mbid.to_owned(),
                }];
                artists::upsert_artist(conn, &input).await?
            }
        };
        taken.push(resolved);
        let ids = [ExternalId {
            namespace: MB_ARTIST.to_owned(),
            value: mbid.to_owned(),
        }];
        attach_and_merge(conn, EntityKind::Artist, resolved, &ids).await?;
    }
    Ok(())
}

/// Returns the value of one canonical namespace from a list of identifiers.
///
/// # Parameters
///
/// - `ids`: the identifiers.
/// - `namespace`: the namespace to look for.
///
/// # Returns
///
/// The first value in that namespace, when one is present.
fn canonical_id<'a>(ids: &'a [ExternalId], namespace: &str) -> Option<&'a str> {
    ids.iter()
        .find(|id| id.namespace == namespace)
        .map(|id| id.value.as_str())
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
