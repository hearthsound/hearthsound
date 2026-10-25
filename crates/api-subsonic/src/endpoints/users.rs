//! The account endpoints: one account's identity, the account listing, and the
//! account-management calls this server answers by pointing elsewhere.
//!
//! A client reads [`get_user`] to learn what the account it signed in as may do,
//! and an administrator reads [`get_users`] to list every account. The roles are
//! the permissions the server actually enforces: downloading and uploading are
//! the account's own flags, and the roles this server has no notion of are
//! reported as off.
//!
//! The four Subsonic calls that create, change, or delete an account are not
//! answered here. Accounts are managed over the native API or from the command
//! line, which can express permissions and quotas that the base protocol has no
//! fields for, so every one of those calls answers error 50 with
//! [`USERS_HELP_URL`] rather than half-implementing a second way to write an
//! account.

use hearthsound_auth::User;

use crate::auth::Call;
use crate::params::Params;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};

/// The music folder id that selects the personal library.
const LIBRARY_FOLDER_ID: i64 = 1;
/// The music folder id that selects the server-wide view.
const SERVER_FOLDER_ID: i64 = 2;

/// Where a client that asked to manage accounts is sent.
///
/// The documentation describes the native API and the command line, because
/// neither `createUser` nor its three siblings is answered here.
pub const USERS_HELP_URL: &str = "https://hearthsound.app/docs/admin/users";

/// Where a client that asked to change a password is sent.
///
/// The client-credentials page covers both halves of that request: the caller's
/// own password over `POST /api/v1/me/password`, and somebody else's through
/// `/api/v1/admin/users`.
pub const PASSWORDS_HELP_URL: &str = "https://hearthsound.app/docs/clients/app-passwords";

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
/// not exist, and a generic error when the account cannot be read.
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

    let folders = browse_folders(user.is_admin);
    call.encode(Response::ok().member("user", user_object(&user, folders)))
}

/// Answers `getUsers`: every account, for an administrator.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `users`, one entry per account, ordered
/// case-insensitively by username. Every entry carries the folders the account
/// may browse, exactly as [`get_user`] reports them.
///
/// # Errors
///
/// Answers Subsonic error 50 for a non-administrator and a generic error when
/// the accounts cannot be read.
pub async fn get_users(call: Call) -> Encoded {
    let principal = call.principal();
    if !principal.is_admin {
        tracing::debug!(
            username = principal.username.as_str(),
            "a non-administrator asked for the account list"
        );
        return call.encode(Response::failed(SubsonicError::not_authorized()));
    }

    let users = match call.state().users().list().await {
        Ok(users) => users,
        Err(error) => {
            return call.encode(Response::failed(crate::browse::failure("getUsers", &error)));
        }
    };
    let entries = users
        .iter()
        .map(|user| user_object(user, browse_folders(user.is_admin)))
        .collect::<Vec<Object>>();
    call.encode(Response::ok().member("users", Object::new().set("user", Array::new(entries))))
}

/// Answers `createUser`, `updateUser`, `deleteUser`, and `changePassword`.
///
/// Accounts are managed over the native API and from the command line, so the
/// base protocol's account writes are not answered here. The answer is the
/// same whatever the caller may do, and it names where the client can read how
/// to manage accounts instead.
///
/// # Parameters
///
/// - `params`: the request's parameters, which are not read.
///
/// # Returns
///
/// A failed envelope carrying Subsonic error 50 and [`USERS_HELP_URL`].
pub async fn unsupported_management(params: Params) -> Encoded {
    params.encode(Response::failed(
        SubsonicError::not_authorized().with_help_url(USERS_HELP_URL),
    ))
}

/// Answers `changePassword`.
///
/// The caller's own password changes over the native API, where the current
/// password is required and every other credential of the account is revoked in
/// the same transaction; somebody else's is an administrator action. The answer
/// is the base protocol's "not authorised" error with the documentation link
/// that covers both.
///
/// # Parameters
///
/// - `params`: the request's parameters, which are not read.
///
/// # Returns
///
/// A failed envelope carrying Subsonic error 50 and [`PASSWORDS_HELP_URL`].
pub async fn unsupported_change_password(params: Params) -> Encoded {
    params.encode(Response::failed(
        SubsonicError::not_authorized().with_help_url(PASSWORDS_HELP_URL),
    ))
}

/// Returns the music folder ids an account may browse.
///
/// The server-wide view is a music folder for administrators only, and the
/// personal library always is.
///
/// # Parameters
///
/// - `is_admin`: whether the account administers the server.
///
/// # Returns
///
/// The folder ids, the personal library first.
fn browse_folders(is_admin: bool) -> Vec<i64> {
    if is_admin {
        vec![LIBRARY_FOLDER_ID, SERVER_FOLDER_ID]
    } else {
        vec![LIBRARY_FOLDER_ID]
    }
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
