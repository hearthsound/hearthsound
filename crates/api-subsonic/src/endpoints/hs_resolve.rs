//! The readable-path vendor endpoint.
//!
//! `hsResolve` turns a path into the entity it names and an entity into its
//! canonical path. A client that opens an artist or an album builds the link
//! from names, so the address it shows never carries a stored id or a plugin's
//! `x_` id. The endpoint is what makes that link work: the client asks which
//! entity a path names, and the answer is the id the browse endpoints take.
//!
//! The endpoint belongs to the `hearthsound` vendor extension, so it is answered
//! only when the request carries `hearthsound=1` and answers Subsonic error 70
//! without it.

use hearthsound_catalog::ResolvedPath;
use hearthsound_core::{ClientId, EntityKind};

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};
use crate::scope;
use crate::value::Object;

/// Answers `hsResolve`: the entity a readable path names.
///
/// One of three requests is answered, in this order: an `id` resolves to the
/// entity's canonical path, an `artist` and an `album` resolve to an album, and
/// an `artist` alone resolves to an artist. A segment that no artist or album
/// answers to is Subsonic error 70, which is what a client shows as "not found".
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a track, an album or an artist,
///   stored or catalog; `artist` and `album` are path segments. The request must
///   carry `hearthsound=1`.
///
/// # Returns
///
/// A successful envelope carrying `resolved` with the entity's `type`, `id` and
/// canonical `path`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// when `id` is malformed or names no such entity, and when the path names no
/// entity; error 10 when the request names neither `id` nor `artist`. A read
/// failure is a generic error.
pub async fn hs_resolve(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let params = call.params();
    let id = params.get("id").filter(|value| !value.is_empty());
    let artist = params.get("artist").filter(|value| !value.is_empty());
    let album = params.get("album").filter(|value| !value.is_empty());
    let scopes = match scope::search_scopes(&call).await {
        Ok(scopes) => scopes,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let catalog = call.state().catalog();
    let resolved = match (id, artist, album) {
        (Some(id), _, _) => {
            let Ok(id) = ClientId::parse(id) else {
                return call.encode(Response::failed(SubsonicError::not_found()));
            };
            catalog.path_of(&id, &scopes).await
        }
        (None, Some(artist), Some(album)) => {
            catalog.resolve_album_path(artist, album, &scopes).await
        }
        (None, Some(artist), None) => catalog.resolve_artist_path(artist, &scopes).await,
        (None, None, _) => {
            return call.encode(Response::failed(SubsonicError::missing_parameter("artist")));
        }
    };
    match resolved {
        Ok(Some(resolved)) => call.encode(Response::ok().member("resolved", answer(resolved))),
        Ok(None) => call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => {
            tracing::warn!(%error, "a path could not be resolved");
            call.encode(Response::failed(SubsonicError::generic()))
        }
    }
}

/// Builds the `resolved` object of an answer.
///
/// # Parameters
///
/// - `resolved`: the entity the path names.
///
/// # Returns
///
/// The object, carrying the entity's type, id and canonical path.
fn answer(resolved: ResolvedPath) -> Object {
    let kind = match resolved.kind {
        EntityKind::Artist => "artist",
        EntityKind::Album => "album",
        _ => "unknown",
    };
    Object::new()
        .set("type", kind)
        .set("id", resolved.id.to_string())
        .set("path", resolved.path)
}
