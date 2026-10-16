//! The `getUser` endpoint: one account's identity and roles.
//!
//! A client reads this to learn what the account it signed in as may do, so the
//! roles are the permissions the server actually enforces: downloading and
//! uploading are the account's own flags, and the roles this server has no
//! notion of are reported as off.

use hearthsound_auth::User;
use hearthsound_config::keys;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};

/// The music folder id that selects the personal library.
const LIBRARY_FOLDER_ID: i64 = 1;
/// The music folder id that selects the server-wide view.
const SERVER_FOLDER_ID: i64 = 2;

/// Answers `getUser`: one account's name, roles, and music folders.
///
/// # Parameters
///
/// - `call`: the authenticated call. `username` names the account.
///
/// # Returns
///
/// A successful envelope carrying `user`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `username` is missing, error 50 when a
/// non-administrator asks for another account, error 70 when the account does
/// not exist, and a generic error when a setting or the account cannot be read.
pub async fn get_user(call: Call) -> Encoded {
    let Some(username) = call.get("username").filter(|name| !name.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter(
            "username",
        )));
    };
    let principal = call.principal();
    if !principal.is_admin && !principal.username.eq_ignore_ascii_case(username) {
        tracing::debug!(username, "a non-administrator asked for another account");
        return call.encode(Response::failed(SubsonicError::not_authorized()));
    }
    let user = match call.state().users().find_by_username(username).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            tracing::debug!(username, "a user request named an unknown account");
            return call.encode(Response::failed(SubsonicError::not_found()));
        }
        Err(error) => {
            return call.encode(Response::failed(crate::browse::failure("getUser", &error)));
        }
    };

    // The server-wide view is a music folder only while it is offered.
    let server_browse = match call
        .state()
        .settings_snapshot()
        .get(keys::PRIVACY_SERVER_BROWSE)
    {
        Ok(value) => value,
        Err(error) => {
            return call.encode(Response::failed(crate::browse::failure("getUser", &error)));
        }
    };
    let folders: Vec<i64> = if server_browse == "disabled" {
        vec![LIBRARY_FOLDER_ID]
    } else {
        vec![LIBRARY_FOLDER_ID, SERVER_FOLDER_ID]
    };

    call.encode(Response::ok().member("user", user_object(&user, folders)))
}

/// Projects one account.
///
/// # Parameters
///
/// - `user`: the account row.
/// - `folders`: the music folder ids the account may browse.
///
/// # Returns
///
/// The user, with the roles this server enforces.
fn user_object(user: &User, folders: Vec<i64>) -> Object {
    Object::new()
        .set("username", user.username.clone())
        // This server has no notion of an account email, and the protocol asks
        // for the member.
        .set("email", "")
        .set("scrobblingEnabled", true)
        .set("adminRole", user.is_admin)
        .set("settingsRole", true)
        .set("downloadRole", user.can_download)
        .set("uploadRole", user.can_upload)
        .set("playlistRole", true)
        .set("coverArtRole", false)
        .set("commentRole", false)
        .set("podcastRole", false)
        .set("streamRole", true)
        .set("jukeboxRole", false)
        .set("shareRole", false)
        .set("videoConversionRole", false)
        .set("folder", Array::new(folders))
}
