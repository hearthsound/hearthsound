//! The `hearthsound` member on track, album and artist objects.
//!
//! A request that carries `hearthsound=1` receives, on every track, album and
//! artist object of its payload, a `hearthsound` object describing the
//! availability and personal state of that item. The decoration is one
//! post-pass over the payload tree, so the endpoints that build those objects
//! stay unaware of it. Without the flag nothing here runs.
//!
//! The state comes from [`hearthsound_library::query::entity_facts`], which is
//! the same read the native pages use, so the two surfaces never disagree about
//! an entity. The one addition is the preferred file's quality, which a client
//! shows beside the state of a track it can play now.

use std::collections::{HashMap, HashSet};

use hearthsound_acquire::repo::{AcquisitionRow, active_for_tracks};
use hearthsound_core::{Availability, EntityId, EntityKind, IdPrefix};
use hearthsound_library::MediaFile;
use hearthsound_library::media_files::preferred_media_files;
use hearthsound_library::query::EntityFacts;
use uuid::Uuid;

use crate::browse::failure;
use crate::response::SubsonicError;
use crate::state::SubsonicState;
use crate::value::{Object, Value};

/// The operation name reported when a decoration read fails.
const OPERATION: &str = "hearthsound member";

/// Appends the `hearthsound` member to every decorable object of `payload`.
///
/// An object is decorable when its `id` member parses as an entity id of kind
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
    if raw.is_empty() {
        return Ok(());
    }
    let mut members = HashMap::new();
    let mut subjects = HashMap::new();
    for id in &raw {
        if let Some(subject) = resolve(state, id).await? {
            subjects.insert(id.clone(), subject);
        }
    }
    if !subjects.is_empty() {
        let facts = facts(state, user, &subjects).await?;
        members.extend(members_for(&facts, &subjects, scores));
    }
    members.extend(playlist_members(state, user, &raw).await?);
    if members.is_empty() {
        return Ok(());
    }
    apply_object(payload, &members);
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
        let Ok(id) = EntityId::parse(raw) else {
            continue;
        };
        if id.kind() != IdPrefix::Playlist {
            continue;
        }
        let playlist = match state.activity().playlists().get(id, user).await {
            Ok(Some(playlist)) => playlist,
            Ok(None) => continue,
            Err(error) => return Err(fail(&error)),
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
    /// The entity kind.
    pub(crate) kind: EntityKind,
    /// The row that stands for the entity now.
    pub(crate) entity: Uuid,
}

/// Maps a client id to its subject, or `None` for ids the decorator skips.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose catalog layer resolves the redirects a
///   merge leaves.
/// - `raw`: the id the client wrote.
///
/// # Errors
///
/// Returns a generic error when the redirect lookup fails.
pub(crate) async fn resolve(
    state: &SubsonicState,
    raw: &str,
) -> Result<Option<Subject>, SubsonicError> {
    let Ok(id) = EntityId::parse(raw) else {
        return Ok(None);
    };
    let Some(kind) = id.kind().entity_kind() else {
        return Ok(None);
    };
    if !decorable(kind) {
        return Ok(None);
    }
    match state.catalog().canonical(kind, id).await {
        Ok(Some(entity)) => Ok(Some(Subject {
            kind,
            entity: entity.uuid(),
        })),
        Ok(None) => Ok(None),
        Err(error) => Err(fail(&error)),
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
    /// The stored state of every subject, keyed by the row's primary key.
    pub(crate) facts: HashMap<Uuid, EntityFacts>,
    /// The preferred media file of every track in scope that has one.
    pub(crate) files: HashMap<EntityId, MediaFile>,
    /// The active acquisition of every track that has one.
    pub(crate) active: HashMap<EntityId, AcquisitionRow>,
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
/// The facts every vendor view of those subjects is built from: the stored
/// availability, personal membership, feedback and rating of each entity, the
/// preferred files of the tracks, and their active acquisitions.
///
/// # Errors
///
/// Returns a generic [`SubsonicError`] when a read fails.
pub(crate) async fn facts(
    state: &SubsonicState,
    user: Uuid,
    subjects: &HashMap<String, Subject>,
) -> Result<Facts, SubsonicError> {
    let mut facts = HashMap::new();
    let mut files = HashMap::new();
    let mut active = HashMap::new();
    for kind in [EntityKind::Track, EntityKind::Album, EntityKind::Artist] {
        let ids: Vec<Uuid> = subjects
            .values()
            .filter(|subject| subject.kind == kind)
            .map(|subject| subject.entity)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if ids.is_empty() {
            continue;
        }
        let found = hearthsound_library::query::entity_facts(state.database(), user, kind, &ids)
            .await
            .map_err(|error| fail(&error))?;
        facts.extend(found);
    }
    let tracks: Vec<EntityId> = subjects
        .values()
        .filter(|subject| subject.kind == EntityKind::Track)
        .map(|subject| EntityId::from_uuid(IdPrefix::Track, subject.entity))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if !tracks.is_empty() {
        let mut conn = state
            .database()
            .readers()
            .acquire()
            .await
            .map_err(|error| fail(&error))?;
        files = preferred_media_files(&mut conn, &tracks)
            .await
            .map_err(|error| fail(&error))?;
        active = active_for_tracks(&mut conn, &tracks)
            .await
            .map_err(|error| fail(&error))?;
    }
    Ok(Facts {
        facts,
        files,
        active,
    })
}

/// Builds the `hearthsound` member of every subject that has one.
///
/// # Parameters
///
/// - `facts`: the batch reads of every subject in scope.
/// - `subjects`: the resolved entities, keyed by the id the client wrote.
/// - `scores`: the relevance of each result, keyed by the id the client wrote.
///
/// # Returns
///
/// The member of each subject, keyed by the id the client wrote. A subject that
/// names nothing stored has no member.
fn members_for(
    facts: &Facts,
    subjects: &HashMap<String, Subject>,
    scores: &HashMap<String, f64>,
) -> HashMap<String, Object> {
    subjects
        .iter()
        .filter_map(|(raw, subject)| {
            member_for(facts, subject).map(|member| {
                let member = match scores.get(raw) {
                    Some(score) => member.set("score", *score),
                    None => member,
                };
                (raw.clone(), member)
            })
        })
        .collect()
}

/// What one subject is, once the facts are read.
///
/// This is the derivation both vendor surfaces share: the `hearthsound` member
/// of a response and the `hsGetAvailability` entry of an availability answer are
/// built from the same standing, so the two never disagree about an entity.
pub(crate) struct Standing {
    /// The stored entity the subject resolved to.
    pub(crate) entity: Uuid,
    /// The availability of the entity, aggregated over its member tracks for an
    /// album or an artist.
    pub(crate) availability: Availability,
    /// Whether the requesting user's personal library holds the entity.
    pub(crate) in_library: bool,
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
/// The standing, or `None` when the row left the database between the
/// resolution and this read.
pub(crate) fn standing(facts: &Facts, subject: &Subject) -> Option<Standing> {
    let fact = facts.facts.get(&subject.entity)?;
    Some(Standing {
        entity: subject.entity,
        availability: fact.availability,
        in_library: fact.in_library,
    })
}

/// Builds the `hearthsound` object of one subject, or `None` when there is none.
fn member_for(facts: &Facts, subject: &Subject) -> Option<Object> {
    let standing = standing(facts, subject)?;
    let fact = facts.facts.get(&standing.entity)?;
    let mut member = base(standing.availability, standing.in_library, fact.disliked);
    if subject.kind != EntityKind::Track {
        return Some(member);
    }
    let track = EntityId::from_uuid(IdPrefix::Track, standing.entity);
    if let Some(row) = facts.active.get(&track) {
        let acquisition = EntityId::from_uuid(IdPrefix::Acquisition, row.id);
        member = member.set("acquisitionId", acquisition.to_string());
    }
    if let Some(file) = facts.files.get(&track) {
        member = member.set("quality", quality(file));
    }
    Some(member)
}

/// The common fields of every member.
fn base(availability: Availability, in_library: bool, disliked: bool) -> Object {
    Object::new()
        .set("availability", availability.as_str())
        .set("inLibrary", in_library)
        .set("disliked", disliked)
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

/// Appends the decorated `hearthsound` member to every matching object, depth
/// first.
fn apply_object(object: &mut Object, members: &HashMap<String, Object>) {
    for (_, value) in object.members_mut() {
        apply_value(value, members);
    }
    let Some(member) = id_of(object).and_then(|id| members.get(id)).cloned() else {
        return;
    };
    // A member the endpoint set itself carries what the standing cannot know; it
    // is kept for the fields the computed member does not state.
    if let Some(existing) = object
        .members()
        .iter()
        .find(|(name, _)| name == "hearthsound")
        .and_then(|(_, value)| match value {
            Value::Object(existing) => Some(existing.clone()),
            _ => None,
        })
    {
        let mut merged = member.clone();
        for (name, value) in existing.members() {
            if !merged.members().iter().any(|(known, _)| known == name) {
                merged = merged.set(name.clone(), value.clone());
            }
        }
        *object = std::mem::take(object).set("hearthsound", merged);
        return;
    }
    *object = std::mem::take(object).set("hearthsound", member);
}

/// The string `id` member of an object, when it has one.
fn id_of(object: &Object) -> Option<&str> {
    object
        .members()
        .iter()
        .find(|(key, _)| key.as_ref() == "id")
        .and_then(|(_, value)| match value {
            Value::Str(text) if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
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
    fn a_nested_song_carries_the_member_of_its_id() {
        let song = Object::new().set("id", "tr_01j9z3k4m5n6p7q8r9s0t1v2w3");
        let album = Object::new()
            .set("id", "al_01j9z3k4m5n6p7q8r9s0t1v2w3")
            .set("song", Array::new([song]));
        let mut payload = Object::new().set("album", album);
        apply_object(
            &mut payload,
            &members(&[
                "al_01j9z3k4m5n6p7q8r9s0t1v2w3",
                "tr_01j9z3k4m5n6p7q8r9s0t1v2w3",
            ]),
        );

        let album = only(&payload, "album");
        assert!(
            attached(album)
                .members()
                .iter()
                .any(|(key, _)| key == "inLibrary")
        );
        let song = only(album, "song");
        assert!(
            attached(song)
                .members()
                .iter()
                .any(|(key, _)| key == "inLibrary")
        );
    }

    #[test]
    fn an_id_without_a_member_is_left_alone() {
        let mut payload = Object::new().set("id", "pl_01j9z3k4m5n6p7q8r9s0t1v2w3");
        apply_object(&mut payload, &members(&["tr_01j9z3k4m5n6p7q8r9s0t1v2w3"]));

        assert!(
            !payload
                .members()
                .iter()
                .any(|(key, _)| key == "hearthsound")
        );
    }
}
