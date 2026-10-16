//! The star endpoints: `star`, `unstar`, `getStarred`, and `getStarred2`.
//!
//! A star is a like: the activity layer records it per user and per entity, and
//! a starred entity is listed whatever its availability, because the like is
//! the only thing the listing reads. An entity named by a catalog id is
//! materialized first, so a client can star what a search returned before it
//! has ever played it.

use std::collections::HashMap;

use hearthsound_activity::Annotation;
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix};
use hearthsound_library::{Artist, LibraryRepository};
use uuid::Uuid;

use crate::auth::Call;
use crate::browse::LoadedAlbum;
use crate::response::{Encoded, Response, SubsonicError};
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
    match result {
        Ok(()) => call.encode(Response::ok()),
        Err(error) => call.encode(Response::failed(browse::failure("a star change", &error))),
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
            entities.push((named.kind, resolve(call, named, value).await?));
        }
    }
    if entities.is_empty() {
        return Err(SubsonicError::missing_parameter("id"));
    }
    Ok(entities)
}

/// Resolves one value of a star request to a stored entity.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `named`: the parameter the value came from.
/// - `value`: the id the client wrote.
///
/// # Returns
///
/// The stored entity's id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the value is not an id of the parameter's
/// kind, when it names no stored entity, or when it names a catalog entity the
/// catalog does not know, and a generic error when a catalog id cannot be
/// materialized.
async fn resolve(call: &Call, named: &Named, value: &str) -> Result<Uuid, SubsonicError> {
    let Ok(id) = ClientId::parse(value) else {
        tracing::debug!(
            parameter = named.parameter,
            "a star request named an unknown id"
        );
        return Err(SubsonicError::not_found());
    };
    match id {
        ClientId::Entity(id) if id.kind() == named.prefix => {
            let library = LibraryRepository::new(call.state().database());
            let stored = match named.kind {
                EntityKind::Track => library.get_track(id).await.map(|row| row.is_some()),
                EntityKind::Album => library.get_album(id).await.map(|row| row.is_some()),
                EntityKind::Artist => library.get_artist(id).await.map(|row| row.is_some()),
                _ => Ok(false),
            };
            let stored = match stored {
                Ok(stored) => stored,
                Err(error) => return Err(browse::failure("a star change", &error)),
            };
            if stored {
                Ok(id.uuid())
            } else {
                tracing::debug!(
                    parameter = named.parameter,
                    "a star request named a stored entity that does not exist"
                );
                Err(SubsonicError::not_found())
            }
        }
        ClientId::Entity(_) => {
            tracing::debug!(
                parameter = named.parameter,
                "a star request named an entity of another kind"
            );
            Err(SubsonicError::not_found())
        }
        ClientId::Catalog(id) if id.kind() == named.kind => materialize(call, &id).await,
        ClientId::Catalog(_) => {
            tracing::debug!(
                parameter = named.parameter,
                "a star request named a catalog entity of another kind"
            );
            Err(SubsonicError::not_found())
        }
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
    let stored = match id.kind() {
        EntityKind::Track => catalog.materialize_track(id).await,
        EntityKind::Album => catalog
            .materialize_album(id)
            .await
            .map(|materialized| materialized.album),
        EntityKind::Artist => catalog.materialize_artist(id).await,
        _ => return Err(SubsonicError::not_found()),
    };
    match stored {
        Ok(stored) => Ok(stored.uuid()),
        Err(error) => Err(catalog::failure(&error)),
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
                &album.detail.album,
                &album.detail.artists,
                &album.detail.tracks,
                &listed.annotations.album(album.detail.album.id),
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
    call.encode(Response::ok().member("starred", starred))
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
                &album.detail.album,
                &album.detail.artists,
                &album.detail.tracks,
                &listed.annotations.album(album.detail.album.id),
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
    call.encode(Response::ok().member("starred2", starred))
}

/// The entities a user liked, with everything the listings project.
struct Starred {
    /// The liked artists, with the album count their listings report.
    artists: Vec<(Artist, i64)>,
    /// The liked albums, with their artists and tracks.
    albums: Vec<LoadedAlbum>,
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
    let album_ids: Vec<EntityId> = albums.iter().map(|album| album.detail.album.id).collect();
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
