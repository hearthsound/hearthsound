//! The `hearthsound` member on track, album and artist objects.
//!
//! A request that carries `hearthsound=1` receives, on every track, album and
//! artist object of its payload, a `hearthsound` object describing the
//! availability and personal state of that item. The decoration is one
//! post-pass over the payload tree, so the endpoints that build those objects
//! stay unaware of it. Without the flag nothing here runs.

use std::collections::{HashMap, HashSet};

use hearthsound_acquire::repo::{AcquisitionRow, active_for_tracks};
use hearthsound_activity::disliked_ids;
use hearthsound_catalog::aliases::provider_namespace;
use hearthsound_core::{Availability, ClientId, EntityId, EntityKind, IdPrefix};
use hearthsound_library::query::{
    album_member_tracks, artist_member_tracks, personal_album_ids, personal_artist_ids,
    personal_track_ids, track_availabilities,
};
use hearthsound_library::{MediaFile, media_files::preferred_media_files};
use uuid::Uuid;

use crate::browse::failure;
use crate::response::SubsonicError;
use crate::state::SubsonicState;
use crate::value::{Object, Value};

/// The operation name reported when a decoration read fails.
const OPERATION: &str = "hearthsound member";

/// Appends the `hearthsound` member to every decorable object of `payload`.
///
/// An object is decorable when its `id` member parses as a client id of kind
/// track, album or artist. Objects with any other id, or without one, are left
/// untouched.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose database and catalog the reads use.
/// - `user`: the account the personal state is read for.
/// - `payload`: the payload to decorate in place.
///
/// # Returns
///
/// `Ok(())` once the payload is decorated, or when it holds nothing to decorate.
///
/// # Errors
///
/// Returns a generic [`SubsonicError`] when a read fails.
pub(crate) async fn decorate(
    state: &SubsonicState,
    user: Uuid,
    payload: &mut Object,
) -> Result<(), SubsonicError> {
    decorate_with_scores(state, user, payload, &HashMap::new()).await
}

/// Appends the `hearthsound` member to every decorable object, with a relevance
/// score on the objects that have one.
///
/// A search response scores each of its results; every other payload passes an
/// empty map and receives the same members as [`decorate`] builds.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose database and catalog the reads use.
/// - `user`: the account the personal state is read for.
/// - `payload`: the payload to decorate in place.
/// - `scores`: the relevance of each result, keyed by the id the object
///   carries; empty when the payload is not a search answer.
///
/// # Returns
///
/// `Ok(())` once the payload is decorated, or when it holds nothing to decorate.
///
/// # Errors
///
/// Returns a generic [`SubsonicError`] when a read fails.
pub(crate) async fn decorate_with_scores(
    state: &SubsonicState,
    user: Uuid,
    payload: &mut Object,
    scores: &HashMap<String, f64>,
) -> Result<(), SubsonicError> {
    let mut raw = HashSet::new();
    collect_ids(payload, &mut raw);
    let mut subjects = HashMap::new();
    for id in &raw {
        if let Some(subject) = resolve(state, id).await? {
            subjects.insert(id.clone(), subject);
        }
    }
    let mut members = if subjects.is_empty() {
        HashMap::new()
    } else {
        members_for(state, user, &subjects, scores).await?
    };
    members.extend(playlist_members(state, user, &raw).await?);
    if members.is_empty() {
        return Ok(());
    }
    apply_object(payload, &members);
    Ok(())
}

/// Builds the `hearthsound` member of every playlist object in the payload.
///
/// A playlist's member reports its lifetime: whether it is temporary, and when
/// it expires. The read goes through the same visibility rule the playlist
/// endpoints use, so only a playlist the caller may see is decorated.
///
/// # Parameters
///
/// - `state`: the endpoint state.
/// - `user`: the account the playlists are read for.
/// - `ids`: every id string the payload holds.
///
/// # Returns
///
/// The member of every playlist id that names a visible playlist, keyed by the
/// id the client wrote.
///
/// # Errors
///
/// Returns a generic [`SubsonicError`] when a read fails.
async fn playlist_members(
    state: &SubsonicState,
    user: Uuid,
    ids: &HashSet<String>,
) -> Result<HashMap<String, Object>, SubsonicError> {
    let mut members = HashMap::new();
    for raw in ids {
        let Ok(ClientId::Entity(id)) = ClientId::parse(raw) else {
            continue;
        };
        if id.kind() != IdPrefix::Playlist {
            continue;
        }
        let playlist = state
            .activity()
            .playlists()
            .get(id, user)
            .await
            .map_err(|error| fail(&error))?;
        let Some(playlist) = playlist else {
            continue;
        };
        let mut member = Object::new().set("temporary", playlist.is_temporary);
        if let Some(expires_at) = playlist.expires_at {
            member = member.set("expiresAt", expires_at);
        }
        members.insert(raw.clone(), member);
    }
    Ok(members)
}

/// The entity one client id names, once resolved.
pub(crate) struct Subject {
    /// The kind of the entity, always track, album or artist.
    pub(crate) kind: EntityKind,
    /// The stored record, when the id names one or a catalog id resolves to one.
    pub(crate) entity: Option<EntityId>,
    /// Whether the id is a catalog id.
    pub(crate) catalog: bool,
}

/// Maps a client id to its subject, or `None` for ids the decorator skips.
pub(crate) async fn resolve(
    state: &SubsonicState,
    raw: &str,
) -> Result<Option<Subject>, SubsonicError> {
    match ClientId::parse(raw) {
        Ok(ClientId::Entity(id)) => Ok(id
            .kind()
            .entity_kind()
            .filter(|kind| decorable(*kind))
            .map(|kind| Subject {
                kind,
                entity: Some(id),
                catalog: false,
            })),
        Ok(ClientId::Catalog(id)) => {
            let kind = id.kind();
            if !decorable(kind) {
                return Ok(None);
            }
            let namespace = provider_namespace(id.slug());
            let entity = state
                .catalog()
                .entity_by_alias(&namespace, kind, id.provider_id())
                .await
                .map_err(|error| fail(&error))?;
            Ok(Some(Subject {
                kind,
                entity,
                catalog: true,
            }))
        }
        Err(_) => Ok(None),
    }
}

/// Reports whether objects of `kind` carry the vendor member.
fn decorable(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Track | EntityKind::Album | EntityKind::Artist
    )
}

/// The batch reads behind every decorated member.
pub(crate) struct Facts {
    /// The stored availability of every track in scope.
    pub(crate) availability: HashMap<EntityId, Availability>,
    /// The preferred media file of every track in scope that has one.
    pub(crate) files: HashMap<EntityId, MediaFile>,
    /// The tracks in the personal scope.
    pub(crate) personal_tracks: HashSet<EntityId>,
    /// The albums in the personal scope.
    pub(crate) personal_albums: HashSet<EntityId>,
    /// The artists in the personal scope.
    pub(crate) personal_artists: HashSet<EntityId>,
    /// The tracks the user disliked.
    pub(crate) disliked_tracks: HashSet<Uuid>,
    /// The albums the user disliked.
    pub(crate) disliked_albums: HashSet<Uuid>,
    /// The artists the user disliked.
    pub(crate) disliked_artists: HashSet<Uuid>,
    /// The active acquisition of every track that has one.
    pub(crate) active: HashMap<EntityId, AcquisitionRow>,
    /// The member tracks of every album in scope.
    pub(crate) album_tracks: HashMap<EntityId, Vec<EntityId>>,
    /// The credited tracks of every artist in scope.
    pub(crate) artist_tracks: HashMap<EntityId, Vec<EntityId>>,
}

/// Runs the batch reads behind every vendor view of `subjects`.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose database the reads use.
/// - `user`: the account the personal state is read for.
/// - `subjects`: the resolved entities, keyed by the id the client wrote.
///
/// # Returns
///
/// The facts every vendor view of those subjects is built from: availability,
/// preferred files, personal membership, dislikes, active acquisitions, and the
/// member tracks of the albums and artists in scope.
///
/// # Errors
///
/// Returns a generic [`SubsonicError`] when a read fails.
pub(crate) async fn facts(
    state: &SubsonicState,
    user: Uuid,
    subjects: &HashMap<String, Subject>,
) -> Result<Facts, SubsonicError> {
    let track_ids = entities(subjects, EntityKind::Track);
    let album_ids = entities(subjects, EntityKind::Album);
    let artist_ids = entities(subjects, EntityKind::Artist);
    let mut conn = state
        .database()
        .readers()
        .acquire()
        .await
        .map_err(|error| fail(&error))?;
    let album_tracks = album_member_tracks(&mut conn, &album_ids)
        .await
        .map_err(|error| fail(&error))?;
    let artist_tracks = artist_member_tracks(&mut conn, &artist_ids)
        .await
        .map_err(|error| fail(&error))?;
    let scope: Vec<EntityId> = track_ids
        .iter()
        .copied()
        .chain(album_tracks.values().flatten().copied())
        .chain(artist_tracks.values().flatten().copied())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let availability = track_availabilities(&mut conn, &scope)
        .await
        .map_err(|error| fail(&error))?;
    let files = preferred_media_files(&mut conn, &scope)
        .await
        .map_err(|error| fail(&error))?;
    let personal_tracks = personal_track_ids(&mut conn, user, &scope)
        .await
        .map_err(|error| fail(&error))?;
    let personal_albums = personal_album_ids(&mut conn, user, &album_ids)
        .await
        .map_err(|error| fail(&error))?;
    let personal_artists = personal_artist_ids(&mut conn, user, &artist_ids)
        .await
        .map_err(|error| fail(&error))?;
    let active = active_for_tracks(&mut conn, &track_ids)
        .await
        .map_err(|error| fail(&error))?;
    let disliked_tracks = disliked_ids(&mut conn, user, EntityKind::Track, &uuids(&track_ids))
        .await
        .map_err(|error| fail(&error))?;
    let disliked_albums = disliked_ids(&mut conn, user, EntityKind::Album, &uuids(&album_ids))
        .await
        .map_err(|error| fail(&error))?;
    let disliked_artists = disliked_ids(&mut conn, user, EntityKind::Artist, &uuids(&artist_ids))
        .await
        .map_err(|error| fail(&error))?;
    let facts = Facts {
        availability,
        files,
        personal_tracks,
        personal_albums,
        personal_artists,
        disliked_tracks,
        disliked_albums,
        disliked_artists,
        active,
        album_tracks,
        artist_tracks,
    };
    Ok(facts)
}

/// Builds the `hearthsound` member of every subject that has one.
///
/// # Parameters
///
/// - `state`: the endpoint state.
/// - `user`: the account the personal state is read for.
/// - `subjects`: the resolved entities, keyed by the id the client wrote.
///
/// # Returns
///
/// The member of each subject, keyed by the id the client wrote. A subject that
/// names nothing stored has no member.
///
/// # Errors
///
/// Returns a generic [`SubsonicError`] when a read fails.
async fn members_for(
    state: &SubsonicState,
    user: Uuid,
    subjects: &HashMap<String, Subject>,
    scores: &HashMap<String, f64>,
) -> Result<HashMap<String, Object>, SubsonicError> {
    let facts = facts(state, user, subjects).await?;
    Ok(subjects
        .iter()
        .filter_map(|(raw, subject)| {
            member_for(&facts, subject).map(|member| {
                let member = match scores.get(raw) {
                    Some(score) => member.set("score", *score),
                    None => member,
                };
                (raw.clone(), member)
            })
        })
        .collect())
}

/// What one subject is, once the facts are read.
///
/// This is the derivation both vendor surfaces share: the `hearthsound` member
/// of a response and the `hsGetAvailability` entry of an availability answer are
/// built from the same standing, so the two never disagree about an entity.
pub(crate) struct Standing<'a> {
    /// The stored entity the subject resolved to, when it resolved to one.
    pub(crate) entity: Option<EntityId>,
    /// The availability of the entity, aggregated over its member tracks for an
    /// album or an artist.
    pub(crate) availability: Availability,
    /// Whether the requesting user's personal library holds the entity.
    pub(crate) in_library: bool,
    /// The member tracks the aggregation read; empty for a track.
    pub(crate) members: &'a [EntityId],
}

/// Derives the standing of one subject from the facts.
///
/// # Parameters
///
/// - `facts`: the batch reads of every subject in scope.
/// - `subject`: the resolved subject.
///
/// # Returns
///
/// The standing, or `None` for a stored track id that names no track. A catalog
/// id that nothing has materialized stands as `remote` and outside every
/// library, which is what a client needs to know before it asks for the entity.
pub(crate) fn standing<'a>(facts: &'a Facts, subject: &Subject) -> Option<Standing<'a>> {
    let Some(id) = subject.entity else {
        return Some(Standing {
            entity: None,
            availability: Availability::Remote,
            in_library: false,
            members: &[],
        });
    };
    match subject.kind {
        EntityKind::Track => {
            let availability = match facts.availability.get(&id) {
                Some(availability) => *availability,
                None if subject.catalog => Availability::Remote,
                None => return None,
            };
            Some(Standing {
                entity: Some(id),
                availability,
                in_library: facts.personal_tracks.contains(&id),
                members: &[],
            })
        }
        EntityKind::Album => {
            let members = facts.album_tracks.get(&id).map_or(&[][..], Vec::as_slice);
            Some(Standing {
                entity: Some(id),
                availability: aggregate(members, facts),
                in_library: facts.personal_albums.contains(&id)
                    || members.iter().any(|t| facts.personal_tracks.contains(t)),
                members,
            })
        }
        EntityKind::Artist => {
            let members = facts.artist_tracks.get(&id).map_or(&[][..], Vec::as_slice);
            Some(Standing {
                entity: Some(id),
                availability: aggregate(members, facts),
                in_library: facts.personal_artists.contains(&id)
                    || members.iter().any(|t| facts.personal_tracks.contains(t)),
                members,
            })
        }
        _ => None,
    }
}

/// Builds the `hearthsound` object of one subject, or `None` when there is none.
fn member_for(facts: &Facts, subject: &Subject) -> Option<Object> {
    let standing = standing(facts, subject)?;
    let Some(id) = standing.entity else {
        return Some(unresolved());
    };
    let disliked = match subject.kind {
        EntityKind::Track => facts.disliked_tracks.contains(&id.uuid()),
        EntityKind::Album => facts.disliked_albums.contains(&id.uuid()),
        EntityKind::Artist => facts.disliked_artists.contains(&id.uuid()),
        _ => false,
    };
    let server = if subject.kind == EntityKind::Track {
        facts.files.contains_key(&id)
            && matches!(
                standing.availability,
                Availability::Local | Availability::Cached
            )
    } else {
        standing
            .members
            .iter()
            .any(|track| facts.files.contains_key(track))
    };
    let mut member = base(
        standing.availability,
        standing.in_library,
        disliked,
        origin(subject.catalog, standing.in_library, server),
    );
    if subject.kind != EntityKind::Track {
        return Some(member);
    }
    if let Some(row) = facts.active.get(&id) {
        let acquisition = EntityId::from_uuid(IdPrefix::Acquisition, row.id);
        member = member.set("acquisitionId", acquisition.to_string());
    }
    if let Some(file) = facts.files.get(&id) {
        member = member.set("quality", quality(file));
    }
    Some(member)
}

/// The member of an id that names nothing stored.
fn unresolved() -> Object {
    base(Availability::Remote, false, false, "catalog")
}

/// The common fields of every member.
fn base(
    availability: Availability,
    in_library: bool,
    disliked: bool,
    origin: &'static str,
) -> Object {
    Object::new()
        .set("availability", availability.as_str())
        .set("inLibrary", in_library)
        .set("disliked", disliked)
        .set("origin", origin)
}

/// The origin of an item: catalog results always report catalog, personal items
/// report library, and other items with a local file report server.
fn origin(catalog: bool, in_library: bool, server: bool) -> &'static str {
    if catalog {
        "catalog"
    } else if in_library {
        "library"
    } else if server {
        "server"
    } else {
        "catalog"
    }
}

/// Aggregates the availability of an album or artist from its member tracks.
fn aggregate(members: &[EntityId], facts: &Facts) -> Availability {
    let states: Vec<Availability> = members
        .iter()
        .filter_map(|track| facts.availability.get(track).copied())
        .collect();
    if states.is_empty() {
        Availability::Remote
    } else if states.contains(&Availability::Acquiring) {
        Availability::Acquiring
    } else if states.iter().all(|state| *state == Availability::Local) {
        Availability::Local
    } else if states
        .iter()
        .any(|state| matches!(state, Availability::Local | Availability::Cached))
    {
        Availability::Cached
    } else if states
        .iter()
        .all(|state| *state == Availability::Unavailable)
    {
        Availability::Unavailable
    } else {
        Availability::Remote
    }
}

/// The quality object of a preferred media file; absent columns are omitted.
fn quality(file: &MediaFile) -> Object {
    let mut quality = Object::new();
    if let Some(format) = &file.format {
        quality = quality.set("format", format.clone());
    }
    if let Some(kbps) = file.bitrate_kbps {
        quality = quality.set("bitrateKbps", kbps);
    }
    if let Some(rate) = file.sample_rate {
        quality = quality.set("sampleRate", rate);
    }
    if let Some(depth) = file.bit_depth {
        quality = quality.set("bitDepth", depth);
    }
    if let Some(lossless) = file.lossless {
        quality = quality.set("lossless", lossless);
    }
    quality
}

/// The distinct stored ids of `subjects` of one kind.
fn entities(subjects: &HashMap<String, Subject>, kind: EntityKind) -> Vec<EntityId> {
    subjects
        .values()
        .filter(|subject| subject.kind == kind)
        .filter_map(|subject| subject.entity)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

/// The UUIDs behind a list of entity ids.
fn uuids(ids: &[EntityId]) -> Vec<Uuid> {
    ids.iter().map(|id| id.uuid()).collect()
}

/// Turns a read failure into the generic browse failure.
fn fail(error: &dyn std::fmt::Display) -> SubsonicError {
    failure(OPERATION, error)
}

/// Collects every `id` string of the payload tree.
fn collect_ids(object: &Object, ids: &mut HashSet<String>) {
    if let Some(id) = id_of(object) {
        ids.insert(id.to_owned());
    }
    for (_, value) in object.members() {
        collect_value(value, ids);
    }
}

/// Collects the `id` strings nested in one value.
fn collect_value(value: &Value, ids: &mut HashSet<String>) {
    match value {
        Value::Object(object) => collect_ids(object, ids),
        Value::Array(array) => {
            for value in array.values() {
                collect_value(value, ids);
            }
        }
        _ => {}
    }
}

/// The string `id` member of an object, when it has one.
fn id_of(object: &Object) -> Option<&str> {
    object
        .members()
        .iter()
        .find(|(name, _)| name.as_ref() == "id")
        .and_then(|(_, value)| match value {
            Value::Str(id) => Some(id.as_str()),
            _ => None,
        })
}

/// Appends the decorated `hearthsound` member to every matching object, depth first.
fn apply_object(object: &mut Object, members: &HashMap<String, Object>) {
    for (_, value) in object.members_mut() {
        apply_value(value, members);
    }
    let member = id_of(object).and_then(|id| members.get(id)).cloned();
    if let Some(member) = member {
        *object = std::mem::take(object).set("hearthsound", member);
    }
}

/// Decorates the objects nested in one value.
fn apply_value(value: &mut Value, members: &HashMap<String, Object>) {
    match value {
        Value::Object(object) => apply_object(object, members),
        Value::Array(array) => {
            for value in array.values_mut() {
                apply_value(value, members);
            }
        }
        _ => {}
    }
}
