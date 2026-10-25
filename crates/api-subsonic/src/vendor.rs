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
use hearthsound_catalog::paths::segment;
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
    apply_object(payload, &members, &PathContext::default());
    Ok(())
}

/// Builds the `hearthsound` member of every playlist object in the payload.
///
/// A playlist's member reports its lifetime: whether it is temporary, when it
/// expires, and, for a playlist a discovery service generated, the display
/// name of the instance that generated it. The read goes through the same
/// visibility rule the playlist endpoints use, so only a playlist the caller
/// may see is decorated.
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
        if let Some(name) = playlist.source_instance_name {
            member = member.set("generatedBy", name);
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

/// The readable paths of the objects that enclose one object.
///
/// A payload does not always repeat what a child needs: a catalog artist's
/// album listing is a list of titles with no credits, so an album object there
/// cannot name the artist its path starts with. The enclosing object knows it,
/// and a child inherits what its parent established.
#[derive(Debug, Clone, Default)]
struct PathContext {
    /// The enclosing artist's segment.
    artist: Option<String>,
    /// The enclosing album's path, `artist/album`.
    album: Option<String>,
}

impl PathContext {
    /// Returns the context the children of an object are read in.
    ///
    /// An artist hands its own segment down and clears the album, because a
    /// track nested in an album it lists belongs to that album, not to the
    /// artist's page. An album hands its own path down.
    ///
    /// # Parameters
    ///
    /// - `object`: the object whose children are about to be decorated.
    ///
    /// # Returns
    ///
    /// The context the children inherit.
    fn under(&self, object: &Object) -> PathContext {
        match object_kind(object) {
            Some(EntityKind::Artist) => PathContext {
                artist: str_member(object, "name").map(segment),
                album: None,
            },
            Some(EntityKind::Album) => {
                let artist = credited_segment(object).or_else(|| self.artist.clone());
                let album = match (&artist, album_title(object)) {
                    (Some(artist), Some(title)) => Some(format!("{artist}/{}", segment(title))),
                    _ => None,
                };
                PathContext { artist, album }
            }
            _ => self.clone(),
        }
    }
}

/// Appends the decorated `hearthsound` member to every matching object, depth first.
fn apply_object(object: &mut Object, members: &HashMap<String, Object>, inherited: &PathContext) {
    let own = inherited.under(object);
    for (_, value) in object.members_mut() {
        apply_value(value, members, &own);
    }
    let Some(member) = id_of(object).and_then(|id| members.get(id)).cloned() else {
        return;
    };
    let member = with_path(object, member, inherited);
    *object = std::mem::take(object).set("hearthsound", member);
}

/// Adds the readable path of the object to its `hearthsound` member.
///
/// The path is read off the object itself, so a listing never pays for a
/// lookup: an artist carries its segment, an album the artist's segment and its
/// own, and a track the segments of its artist and its album. What the object
/// does not carry is inherited from the object that encloses it, and a path
/// nothing establishes is left out rather than invented.
///
/// # Parameters
///
/// - `object`: the payload object the member belongs to.
/// - `member`: the member built from the entity's standing.
/// - `inherited`: the paths the enclosing objects established.
///
/// # Returns
///
/// The member, with `path` on an artist and an album and `artistPath` and
/// `albumPath` on a track.
fn with_path(object: &Object, member: Object, inherited: &PathContext) -> Object {
    match object_kind(object) {
        Some(EntityKind::Artist) => match str_member(object, "name") {
            Some(name) => member.set("path", segment(name)),
            None => member,
        },
        Some(EntityKind::Album) => {
            let artist = credited_segment(object).or_else(|| inherited.artist.clone());
            match (artist, album_title(object)) {
                (Some(artist), Some(title)) => {
                    member.set("path", format!("{artist}/{}", segment(title)))
                }
                _ => member,
            }
        }
        Some(EntityKind::Track) => {
            let mut member = member;
            if let Some(artist) = credited_segment(object).or_else(|| inherited.artist.clone()) {
                member = member.set("artistPath", artist);
            }
            if let Some(album) = track_album_path(object).or_else(|| inherited.album.clone()) {
                member = member.set("albumPath", album);
            }
            member
        }
        _ => member,
    }
}

/// The entity kind an object's id names, when it names one of the decorable
/// kinds.
///
/// # Parameters
///
/// - `object`: the payload object.
fn object_kind(object: &Object) -> Option<EntityKind> {
    match ClientId::parse(id_of(object)?) {
        Ok(ClientId::Entity(entity)) => entity.kind().entity_kind(),
        Ok(ClientId::Catalog(catalog)) => Some(catalog.kind()),
        Err(_) => None,
    }
}

/// The album's title, as an album object spells it.
///
/// # Parameters
///
/// - `object`: the album object.
fn album_title(object: &Object) -> Option<&str> {
    str_member(object, "name").or_else(|| str_member(object, "title"))
}

/// The segment of the first artist an artist or album object credits.
///
/// # Parameters
///
/// - `object`: the artist or album object.
fn credited_segment(object: &Object) -> Option<String> {
    first_name(object, "artists")
        .or_else(|| str_member(object, "artist"))
        .map(segment)
}

/// The album path a track object carries, `artist/album`.
///
/// # Parameters
///
/// - `object`: the track object.
fn track_album_path(object: &Object) -> Option<String> {
    let album = str_member(object, "album")?;
    let artist = first_name(object, "albumArtists")
        .or_else(|| first_name(object, "artists"))
        .or_else(|| str_member(object, "artist"))?;
    Some(format!("{}/{}", segment(artist), segment(album)))
}

/// The non-empty string member of an object, when it has one.
fn str_member<'a>(object: &'a Object, name: &str) -> Option<&'a str> {
    object
        .members()
        .iter()
        .find(|(key, _)| key.as_ref() == name)
        .and_then(|(_, value)| match value {
            Value::Str(text) if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
}

/// The `name` of the first object of an array member.
fn first_name<'a>(object: &'a Object, array: &str) -> Option<&'a str> {
    let value = object
        .members()
        .iter()
        .find(|(key, _)| key.as_ref() == array)
        .map(|(_, value)| value)?;
    match value {
        Value::Array(items) => match items.values().first()? {
            Value::Object(first) => str_member(first, "name"),
            _ => None,
        },
        _ => None,
    }
}

/// The string `id` member of an object, when it has one.
fn id_of(object: &Object) -> Option<&str> {
    str_member(object, "id")
}

/// Decorates the objects nested in one value.
fn apply_value(value: &mut Value, members: &HashMap<String, Object>, inherited: &PathContext) {
    match value {
        Value::Object(object) => apply_object(object, members, inherited),
        Value::Array(array) => {
            for value in array.values_mut() {
                apply_value(value, members, inherited);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Array;

    /// The member of the given ids, all with the same standing.
    ///
    /// # Parameters
    ///
    /// - `ids`: the ids the payload carries.
    fn members(ids: &[&str]) -> HashMap<String, Object> {
        ids.iter()
            .map(|id| ((*id).to_owned(), Object::new().set("inLibrary", true)))
            .collect()
    }

    /// Returns the first object of a member, whether it is an object or a list.
    ///
    /// # Parameters
    ///
    /// - `object`: the object to read.
    /// - `name`: the member's name.
    fn only<'a>(object: &'a Object, name: &str) -> &'a Object {
        let Some(value) = object
            .members()
            .iter()
            .find(|(key, _)| key.as_ref() == name)
            .map(|(_, value)| value)
        else {
            panic!("the payload has no {name} member");
        };
        match value {
            Value::Object(nested) => nested,
            Value::Array(items) => match items.values().first() {
                Some(Value::Object(first)) => first,
                _ => panic!("the first item of {name} is not an object"),
            },
            _ => panic!("the member {name} is neither an object nor a list"),
        }
    }

    /// Returns the `hearthsound` member of an object.
    ///
    /// # Parameters
    ///
    /// - `object`: the decorated object.
    fn attached(object: &Object) -> &Object {
        let Some(Value::Object(member)) = object
            .members()
            .iter()
            .find(|(key, _)| key.as_ref() == "hearthsound")
            .map(|(_, value)| value)
        else {
            panic!("the object carries no hearthsound member");
        };
        member
    }

    #[test]
    fn an_album_without_credits_inherits_the_artist_path() {
        let album = Object::new()
            .set("id", "x_deezer_a_d1jprv3f5ngprrkndmpk2")
            .set("name", "KID A MNESIA");
        let artist = Object::new()
            .set("id", "x_deezer_r_d1jprv3f5ngq4x39edt2tc8")
            .set("name", "Radiohead")
            .set("album", Array::new([album]));
        let mut payload = Object::new().set("artist", artist);
        apply_object(
            &mut payload,
            &members(&[
                "x_deezer_r_d1jprv3f5ngq4x39edt2tc8",
                "x_deezer_a_d1jprv3f5ngprrkndmpk2",
            ]),
            &PathContext::default(),
        );

        let artist = only(&payload, "artist");
        assert_eq!(str_member(attached(artist), "path"), Some("radiohead"));
        let album = only(artist, "album");
        assert_eq!(
            str_member(attached(album), "path"),
            Some("radiohead/kid-a-mnesia"),
            "the listing carries no credits, so the album takes the artist's segment"
        );
    }

    #[test]
    fn a_track_inherits_the_album_and_artist_it_sits_under() {
        let song = Object::new()
            .set("id", "x_deezer_t_d1jprv3f5nt74rb3dcpk2")
            .set("title", "Everything in Its Right Place");
        let credit = Object::new().set("name", "Radiohead");
        let album = Object::new()
            .set("id", "x_deezer_a_d1jprv3f5ngprrkndmpk2")
            .set("name", "Kid A")
            .set("artists", Array::new([credit]))
            .set("song", Array::new([song]));
        let mut payload = Object::new().set("album", album);
        apply_object(
            &mut payload,
            &members(&[
                "x_deezer_a_d1jprv3f5ngprrkndmpk2",
                "x_deezer_t_d1jprv3f5nt74rb3dcpk2",
            ]),
            &PathContext::default(),
        );

        let album = only(&payload, "album");
        let song = only(album, "song");
        let member = attached(song);
        assert_eq!(str_member(member, "artistPath"), Some("radiohead"));
        assert_eq!(str_member(member, "albumPath"), Some("radiohead/kid-a"));
    }
}
