//! The system endpoints: liveness, licensing, extensions, token information,
//! the virtual music folders, and the jukebox this server does not have.

use hearthsound_auth::Credential;

use crate::auth::Call;
use crate::params::Params;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::value::{Array, Object};

/// The music folder holding the requesting user's personal library.
const LIBRARY_FOLDER: (i64, &str) = (1, "My Library");

/// The music folder holding everything stored on the server.
const SERVER_FOLDER: (i64, &str) = (2, "Server");

/// The OpenSubsonic extensions this server advertises.
///
/// An extension is listed here only once the endpoint or the fields it promises
/// are implemented, because advertising an extension that does nothing is worse
/// for a client than omitting it.
const EXTENSIONS: [(&str, &[i64]); 8] = [
    ("apiKeyAuthentication", &[1]),
    ("formPost", &[1]),
    ("hearthsound", &[1]),
    ("indexBasedQueue", &[1]),
    ("playbackReport", &[1]),
    ("topSongsByArtistId", &[1]),
    ("transcodeOffset", &[1]),
    ("transcoding", &[1]),
];

/// Answers `ping`: the protocol's liveness check.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// An empty successful envelope.
pub async fn ping(call: Call) -> Encoded {
    call.encode(Response::ok())
}

/// Answers `getLicense`: the licensing check.
///
/// Hearthsound has no license tiers, so the base protocol's `valid` field is
/// always true.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `license`.
pub async fn get_license(call: Call) -> Encoded {
    call.encode(Response::ok().member("license", Object::new().set("valid", true)))
}

/// Answers `getOpenSubsonicExtensions`: the extensions this server supports.
///
/// This is the one endpoint that must not require authentication, because a
/// client asks for it before it has credentials to give.
///
/// # Parameters
///
/// - `params`: the request's parameters.
///
/// # Returns
///
/// A successful envelope carrying `openSubsonicExtensions`.
pub async fn get_open_subsonic_extensions(params: Params) -> Encoded {
    let extensions = EXTENSIONS.iter().map(|(name, versions)| {
        Object::new()
            .set("name", *name)
            .set("versions", Array::new(versions.iter().copied()))
    });
    params.encode(Response::ok().member(
        "openSubsonicExtensions",
        Array::named("openSubsonicExtension", extensions),
    ))
}

/// Answers `tokenInfo`: what the presented API key belongs to.
///
/// The endpoint exists to validate an API key and to tell a client which user a
/// key speaks for, so a request authenticated with anything but a key has
/// nothing to report and is answered like an invalid key.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `tokenInfo`, or error 44 when the call was
/// authenticated with something other than an API key.
pub async fn token_info(call: Call) -> Encoded {
    if !matches!(call.principal().credential, Credential::ApiKey { .. }) {
        tracing::debug!("tokenInfo was called with a credential that is not an API key");
        return call.encode(Response::failed(SubsonicError::invalid_api_key()));
    }
    call.encode(Response::ok().member(
        "tokenInfo",
        Object::new().set("username", call.principal().username.clone()),
    ))
}

/// Answers `getMusicFolders`: the virtual folders a client may browse.
///
/// The two folders are the requesting user's personal library and the whole
/// server. The second one is listed for administrators only, and a browse
/// request naming it is answered empty for everyone else, so a client never
/// learns of content it may not see.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `musicFolders`.
pub async fn get_music_folders(call: Call) -> Encoded {
    let mut folders = vec![folder(LIBRARY_FOLDER)];
    if call.principal().is_admin {
        folders.push(folder(SERVER_FOLDER));
    }

    call.encode(Response::ok().member(
        "musicFolders",
        Object::new().set("musicFolder", Array::new(folders)),
    ))
}

/// Answers `jukeboxControl`: this server has no jukebox.
///
/// The endpoint is answered with a generic error (code 0) rather than error
/// 70, because clients probe it after logging in to learn whether to offer
/// jukebox controls, and they surface every other error code to the user as a
/// failure. `getUser` reports `jukeboxRole` false for the same reason.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A failed envelope with error 0.
pub async fn jukebox_control(call: Call) -> Encoded {
    call.encode(Response::failed(SubsonicError::new(
        codes::GENERIC,
        "This server has no jukebox.",
    )))
}

/// Builds one music folder member.
///
/// # Parameters
///
/// - `folder`: the folder's id and name.
///
/// # Returns
///
/// The member, with the id first.
fn folder(folder: (i64, &str)) -> Object {
    Object::new().set("id", folder.0).set("name", folder.1)
}
