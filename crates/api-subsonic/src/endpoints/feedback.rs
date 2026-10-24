//! The feedback endpoints: `star`, `unstar`, `getStarred`, `getStarred2`, and
//! `setRating`.
//!
//! A star is a like: the activity layer records it per user and per entity, and
//! a starred entity is listed whatever its availability, because the like is
//! the only thing the listing reads. An entity named by a catalog id is
//! materialized first, so a client can star what a search returned before it
//! has ever played it.

use std::collections::HashMap;

use hearthsound_acquire::adopt::acquire_saved;
use hearthsound_activity::Annotation;
use hearthsound_config::keys;
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix};
use hearthsound_library::query::AlbumDetail;
use hearthsound_library::{Artist, LibraryRepository};
use uuid::Uuid;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::value::{Array, Object};
use crate::{browse, catalog, projection};

/// One parameter a star request names entities of one kind with.
struct Named {
    /// The parameter name.
    parameter: &'static str,
    /// The kind of entity it names.
    kind: EntityKind,
    /// The prefix the kind's stored ids carry.
    prefix: IdPrefix,
}

/// The parameters `star` and `unstar` read, in the order they are read.
const NAMED: [Named; 3] = [
    Named {
        parameter: "id",
        kind: EntityKind::Track,
        prefix: IdPrefix::Track,
    },
    Named {
        parameter: "albumId",
        kind: EntityKind::Album,
        prefix: IdPrefix::Album,
    },
    Named {
        parameter: "artistId",
        kind: EntityKind::Artist,
        prefix: IdPrefix::Artist,
    },
];

/// Answers `star`: records the user's like of the named entities.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id`, `albumId`, and `artistId` may each
///   be repeated, and each value is a stored or catalog id.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when the request names no entity, error 70 when a
/// value is not an id of the parameter's kind or names no stored entity, and a
/// generic error carrying the catalog's message when a catalog id cannot be
/// materialized.
pub async fn star(call: Call) -> Encoded {
    toggle(&call, true).await
}

/// Answers `unstar`: removes the user's like of the named entities.
///
/// The parameters and the errors are the ones [`star`] documents.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`star`] documents.
pub async fn unstar(call: Call) -> Encoded {
    toggle(&call, false).await
}

/// Records or removes the user's likes of the entities a request names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `like`: whether the entities are starred or unstarred.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`star`] documents.
async fn toggle(call: &Call, like: bool) -> Encoded {
    let entities = match named(call).await {
        Ok(entities) => entities,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    let activity = call.state().activity();
    let result = if like {
        activity.star(user, &entities).await
    } else {
        activity.unstar(user, &entities).await
    };
    if let Err(error) = result {
        return call.encode(Response::failed(browse::failure("a star change", &error)));
    }
    if like {
        adopt_saved(call, user, &entities).await;
    }
    call.encode(Response::ok())
}

/// Acquires the remote tracks of what a user just liked or saved.
///
/// A like and a save are adoptions (`library.acquire_on_save`), so a liked or
/// saved track or album that is not stored is acquired at priority `queue` under
/// the user's permission and quota. The change is already recorded; a refusal or
/// an error here is only logged.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `user`: the user who liked or saved the entities.
/// - `entities`: the liked or saved entities.
pub(super) async fn adopt_saved(call: &Call, user: Uuid, entities: &[(EntityKind, Uuid)]) {
    let Some(prefetch) = call.state().prefetch() else {
        return;
    };
    let named: Vec<(EntityKind, EntityId)> = entities
        .iter()
        .filter_map(|(kind, id)| {
            let prefix = match kind {
                EntityKind::Track => IdPrefix::Track,
                EntityKind::Album => IdPrefix::Album,
                _ => return None,
            };
            Some((*kind, EntityId::from_uuid(prefix, *id)))
        })
        .collect();
    if named.is_empty() {
        return;
    }
    if let Err(error) = acquire_saved(
        prefetch.orchestrator(),
        user,
        call.principal().can_acquire,
        &named,
    )
    .await
    {
        tracing::warn!(%user, %error, "could not acquire what a user liked");
    }
}

/// Reads the entities a star request names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// One entry per value, in request order, as the kind and the stored id.
///
/// # Errors
///
/// Answers Subsonic error 10 when the request names no entity, error 70 when a
/// value is not an id of the parameter's kind or names no stored entity, and a
/// generic error carrying the catalog's message when a catalog id cannot be
/// materialized.
async fn named(call: &Call) -> Result<Vec<(EntityKind, Uuid)>, SubsonicError> {
    let mut entities = Vec::new();
    for named in &NAMED {
        for value in call.params().get_all(named.parameter) {
            if value.is_empty() {
                continue;
            }
            let (kind, id) = resolve(
                call,
                named.parameter,
                value,
                Some((named.kind, named.prefix)),
            )
            .await?;
            entities.push((kind, id));
        }
    }
    if entities.is_empty() {
        return Err(SubsonicError::missing_parameter("id"));
    }
    Ok(entities)
}

/// Resolves one id a request wrote to a stored entity.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `parameter`: the parameter the value came from, for the log.
/// - `value`: the id the client wrote.
/// - `expected`: the kind and prefix the request fixes, when its parameter names
///   one kind of entity; `None` when the id itself decides the kind.
///
/// # Returns
///
/// The kind of the entity and its stored id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the value is not an id, when it is not an id
/// of the kind the request expects, when it names a kind no entity has, when it
/// names no stored entity, or when it names a catalog entity the catalog does
/// not know, and a generic error when a catalog id cannot be materialized.
pub(super) async fn resolve(
    call: &Call,
    parameter: &str,
    value: &str,
    expected: Option<(EntityKind, IdPrefix)>,
) -> Result<(EntityKind, Uuid), SubsonicError> {
    let Ok(id) = ClientId::parse(value) else {
        tracing::debug!(parameter, "a feedback request named an unknown id");
        return Err(SubsonicError::not_found());
    };
    let stored = match &id {
        ClientId::Entity(entity) => match kind_of(entity.kind()) {
            Some(named) if expected.is_none_or(|expected| expected == named) => named,
            _ => {
                tracing::debug!(
                    parameter,
                    "a feedback request named an entity of another kind"
                );
                return Err(SubsonicError::not_found());
            }
        },
        ClientId::Catalog(catalog) => {
            let kind = catalog.kind();
            if expected.is_some_and(|(expected, _)| expected != kind) {
                tracing::debug!(
                    parameter,
                    "a feedback request named a catalog entity of another kind"
                );
                return Err(SubsonicError::not_found());
            }
            return materialize(call, catalog).await.map(|id| (kind, id));
        }
    };

    let library = LibraryRepository::new(call.state().database());
    let found = match stored.0 {
        EntityKind::Track => library
            .get_track(stored_id(&id))
            .await
            .map(|row| row.is_some()),
        EntityKind::Album => library
            .get_album(stored_id(&id))
            .await
            .map(|row| row.is_some()),
        EntityKind::Artist => library
            .get_artist(stored_id(&id))
            .await
            .map(|row| row.is_some()),
        _ => Ok(false),
    };
    let found = match found {
        Ok(found) => found,
        Err(error) => return Err(browse::failure("a feedback change", &error)),
    };
    if !found {
        tracing::debug!(
            parameter,
            "a feedback request named a stored entity that does not exist"
        );
        return Err(SubsonicError::not_found());
    }
    Ok((stored.0, stored_id(&id).uuid()))
}

/// Returns the id of a parsed entity id.
///
/// # Parameters
///
/// - `id`: the parsed id, which the caller has already matched as an entity.
///
/// # Returns
///
/// The entity id.
fn stored_id(id: &ClientId) -> EntityId {
    match id {
        ClientId::Entity(id) => *id,
        ClientId::Catalog(_) => EntityId::new(IdPrefix::Track),
    }
}

/// Returns the kind and prefix a stored id names.
///
/// # Parameters
///
/// - `prefix`: the prefix the id carries.
///
/// # Returns
///
/// The kind of entity it names, or `None` for a prefix no entity uses.
fn kind_of(prefix: IdPrefix) -> Option<(EntityKind, IdPrefix)> {
    match prefix {
        IdPrefix::Track => Some((EntityKind::Track, IdPrefix::Track)),
        IdPrefix::Album => Some((EntityKind::Album, IdPrefix::Album)),
        IdPrefix::Artist => Some((EntityKind::Artist, IdPrefix::Artist)),
        _ => None,
    }
}

/// Stores one catalog entity and returns its stored id.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the catalog layer.
/// - `id`: the catalog id.
///
/// # Returns
///
/// The stored entity's id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the catalog does not know the entity and a
/// generic error carrying the catalog's message when it cannot be stored.
async fn materialize(call: &Call, id: &CatalogId) -> Result<Uuid, SubsonicError> {
    let catalog = call.state().catalog();
    match hearthsound_catalog::materialize_entity(catalog, id).await {
        Ok(stored) => Ok(stored.uuid()),
        Err(error) => Err(catalog::failure(&error)),
    }
}

/// Answers `setRating`: records the user's rating of one entity.
///
/// A rating is per user and per entity: `0` clears it and `1` to `5` set it.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a stored or catalog track,
///   album, or artist, and `rating` is the value from `0` to `5`.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` or `rating` is missing, error 0 when
/// `rating` is not a number from 0 to 5, error 70 when `id` is not an id of a
/// kind a rating applies to or names no stored entity, and a generic error
/// carrying the catalog's message when a catalog id cannot be materialized.
pub async fn set_rating(call: Call) -> Encoded {
    let Some(value) = call.get("id").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let Some(rating) = call.get("rating").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("rating")));
    };
    let rating = match rating.parse::<u8>() {
        Ok(rating) if rating <= 5 => rating,
        _ => {
            return call.encode(Response::failed(SubsonicError::new(
                codes::GENERIC,
                "A rating is a number from 0 to 5.",
            )));
        }
    };
    let (kind, id) = match resolve(&call, "id", value, None).await {
        Ok(resolved) => resolved,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    // A failure to read the setting falls back to its default, which keeps a
    // rating a rating.
    let one_is_dislike = call
        .state()
        .settings_snapshot()
        .get(keys::SUBSONIC_RATING_ONE_IS_DISLIKE)
        .unwrap_or(false);
    let activity = call.state().activity();
    let result = if one_is_dislike {
        activity
            .set_rating_counting_dislike(user, kind, id, rating)
            .await
    } else {
        activity.set_rating(user, kind, id, rating).await
    };
    match result {
        Ok(()) => call.encode(Response::ok()),
        Err(error) => call.encode(Response::failed(browse::failure("a rating change", &error))),
    }
}

/// Answers `getStarred`: the entities the user liked, in the older shape.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `starred`, or a failed envelope.
///
/// # Errors
///
/// Answers a generic error when a read fails.
pub async fn get_starred(call: Call) -> Encoded {
    let listed = match starred(&call).await {
        Ok(listed) => listed,
        Err(error) => return call.encode(Response::failed(error)),
    };

    let artists: Vec<Object> = listed
        .artists
        .iter()
        .map(|(artist, _)| {
            projection::starred_artist_ref(artist, &listed.annotations.artist(artist.id))
        })
        .collect();
    let albums: Vec<Object> = listed
        .albums
        .iter()
        .map(|album| {
            projection::album_dir(
                &album.album,
                &album.artists,
                &album.tracks,
                &listed.annotations.album(album.album.id),
            )
        })
        .collect();
    let songs: Vec<Object> = listed
        .tracks
        .iter()
        .map(|track| {
            projection::child(
                &track.view,
                track.file.as_ref(),
                &track.album_artists,
                &listed.annotations.track(track.view.track.id),
            )
        })
        .collect();

    let starred = Object::new()
        .set("artist", Array::new(artists))
        .set("album", Array::new(albums))
        .set("song", Array::new(songs));
    call.respond(Response::ok().member("starred", starred))
        .await
}

/// Answers `getStarred2`: the entities the user liked, as ID3 entities.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `starred2`, or a failed envelope.
///
/// # Errors
///
/// Answers a generic error when a read fails.
pub async fn get_starred2(call: Call) -> Encoded {
    let listed = match starred(&call).await {
        Ok(listed) => listed,
        Err(error) => return call.encode(Response::failed(error)),
    };

    let artists: Vec<Object> = listed
        .artists
        .iter()
        .map(|(artist, album_count)| {
            projection::artist_id3(artist, *album_count, &listed.annotations.artist(artist.id))
        })
        .collect();
    let albums: Vec<Object> = listed
        .albums
        .iter()
        .map(|album| {
            projection::album_id3(
                &album.album,
                &album.artists,
                &album.tracks,
                &listed.annotations.album(album.album.id),
            )
        })
        .collect();
    let songs: Vec<Object> = listed
        .tracks
        .iter()
        .map(|track| {
            projection::child(
                &track.view,
                track.file.as_ref(),
                &track.album_artists,
                &listed.annotations.track(track.view.track.id),
            )
        })
        .collect();

    let starred = Object::new()
        .set("artist", Array::new(artists))
        .set("album", Array::new(albums))
        .set("song", Array::new(songs));
    call.respond(Response::ok().member("starred2", starred))
        .await
}

/// The entities a user liked, with everything the listings project.
struct Starred {
    /// The liked artists, with the album count their listings report.
    artists: Vec<(Artist, i64)>,
    /// The liked albums, with their artists and tracks.
    albums: Vec<AlbumDetail>,
    /// The liked songs, with their files and album artists.
    tracks: Vec<browse::LoadedTrack>,
    /// The user's annotations for every listed entity.
    annotations: Annotations,
}

/// The user's annotations for the entities of one starred listing.
#[derive(Debug, Default)]
struct Annotations {
    /// The artist annotations, keyed by artist id.
    artists: HashMap<Uuid, Annotation>,
    /// The album annotations, keyed by album id.
    albums: HashMap<Uuid, Annotation>,
    /// The track annotations, keyed by track id.
    tracks: HashMap<Uuid, Annotation>,
}

impl Annotations {
    /// Returns the annotation of an artist.
    ///
    /// # Parameters
    ///
    /// - `id`: the artist's id.
    ///
    /// # Returns
    ///
    /// The user's annotation, or the default when the user has none.
    fn artist(&self, id: EntityId) -> Annotation {
        self.artists.get(&id.uuid()).copied().unwrap_or_default()
    }

    /// Returns the annotation of an album.
    ///
    /// # Parameters
    ///
    /// - `id`: the album's id.
    ///
    /// # Returns
    ///
    /// The user's annotation, or the default when the user has none.
    fn album(&self, id: EntityId) -> Annotation {
        self.albums.get(&id.uuid()).copied().unwrap_or_default()
    }

    /// Returns the annotation of a track.
    ///
    /// # Parameters
    ///
    /// - `id`: the track's id.
    ///
    /// # Returns
    ///
    /// The user's annotation, or the default when the user has none.
    fn track(&self, id: EntityId) -> Annotation {
        self.tracks.get(&id.uuid()).copied().unwrap_or_default()
    }
}

/// Reads the entities a user liked, with their projections' inputs.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The liked artists, albums, and songs, each with the detail its projection
/// reads, and the user's annotations for them.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn starred(call: &Call) -> Result<Starred, SubsonicError> {
    let database = call.state().database();
    let user = call.principal().user_id.uuid();
    let mut conn = match database.readers().acquire().await {
        Ok(conn) => conn,
        Err(error) => return Err(browse::failure("getStarred", &error)),
    };
    let liked = match hearthsound_library::query::starred(&mut conn, user).await {
        Ok(liked) => liked,
        Err(error) => return Err(browse::failure("getStarred", &error)),
    };
    drop(conn);

    let mut artists = Vec::with_capacity(liked.artists.len());
    for artist in liked.artists {
        let album_count = match browse::artist_any(database, artist.id).await {
            Ok(Some(detail)) => detail.albums.len() as i64,
            Ok(None) => 0,
            Err(error) => return Err(browse::failure("getStarred", &error)),
        };
        artists.push((artist, album_count));
    }

    let mut albums = Vec::with_capacity(liked.albums.len());
    for album in liked.albums {
        match browse::album_any(database, album.id).await {
            Ok(Some(loaded)) => albums.push(loaded),
            Ok(None) => {}
            Err(error) => return Err(browse::failure("getStarred", &error)),
        }
    }

    let mut tracks = Vec::with_capacity(liked.tracks.len());
    for view in liked.tracks {
        match browse::track_any(database, view.track.id).await {
            Ok(Some(loaded)) => tracks.push(loaded),
            Ok(None) => {}
            Err(error) => return Err(browse::failure("getStarred", &error)),
        }
    }

    let artist_ids: Vec<EntityId> = artists.iter().map(|(artist, _)| artist.id).collect();
    let album_ids: Vec<EntityId> = albums.iter().map(|album| album.album.id).collect();
    let track_ids: Vec<EntityId> = tracks.iter().map(|track| track.view.track.id).collect();
    let annotations = match annotations_for(call, &artist_ids, &album_ids, &track_ids).await {
        Ok(annotations) => annotations,
        Err(error) => return Err(browse::failure("getStarred", &error)),
    };

    Ok(Starred {
        artists,
        albums,
        tracks,
        annotations,
    })
}

/// Loads the user's annotations for the entities of one starred listing.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `artist_ids`: the artists of the listing.
/// - `album_ids`: the albums of the listing.
/// - `track_ids`: the songs of the listing.
///
/// # Returns
///
/// One read per kind, keyed by entity id.
///
/// # Errors
///
/// Returns [`ActivityError`] when a read fails.
async fn annotations_for(
    call: &Call,
    artist_ids: &[EntityId],
    album_ids: &[EntityId],
    track_ids: &[EntityId],
) -> Result<Annotations, hearthsound_activity::ActivityError> {
    let user = call.principal().user_id.uuid();
    let state = call.state();
    Ok(Annotations {
        artists: browse::annotations(state, user, EntityKind::Artist, &plain(artist_ids)).await?,
        albums: browse::annotations(state, user, EntityKind::Album, &plain(album_ids)).await?,
        tracks: browse::annotations(state, user, EntityKind::Track, &plain(track_ids)).await?,
    })
}

/// Returns the plain ids of some client ids.
///
/// # Parameters
///
/// - `ids`: the client ids.
///
/// # Returns
///
/// The ids the annotation read keys on.
fn plain(ids: &[EntityId]) -> Vec<Uuid> {
    ids.iter().map(EntityId::uuid).collect()
}
