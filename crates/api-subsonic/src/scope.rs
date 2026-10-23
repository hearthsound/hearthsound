//! The browser scope a request reads.
//!
//! The two music folders `getMusicFolders` advertises are virtual: folder `1`
//! is the requesting user's personal library and folder `2` is the server-wide
//! view, which holds every stored track and belongs to administrators. An
//! explicit `musicFolderId` selects one of them; a request without one reads the
//! user's `subsonic.default_scope` override, falling back to the server-wide
//! value of that setting. The server view is answered empty for everyone else,
//! so a non-administrator who selects it reads nothing.
//!
//! The override is read through [`hearthsound_auth::UserSettings`], which is the
//! only layer that knows a user's value, and the resolution rule (the user's
//! value wins, otherwise the server's) lives here so every browse endpoint
//! applies the same one.
//!
//! The resolved [`Scope`] is what the library crate's queries filter by, so
//! this module decides nothing about the content itself.

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_library::query::Scope;

use crate::auth::Call;
use crate::browse;
use crate::response::SubsonicError;

/// The music folder id that selects the personal library.
const LIBRARY_FOLDER_ID: &str = "1";
/// The music folder id that selects the server-wide view.
const SERVER_FOLDER_ID: &str = "2";
/// The `subsonic.default_scope` value that selects the server-wide view.
const SERVER_SCOPE: &str = "server";

/// Resolves the scope a browse request reads.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `musicFolderId` parameter and
///   principal decide the scope.
///
/// # Returns
///
/// The scope. An explicit `musicFolderId` of `1` is the principal's personal
/// library and of `2` is the server-wide view, which holds every stored track
/// for an administrator and is empty for everyone else. Without a parameter the
/// user's `subsonic.default_scope` decides, and a non-administrator always
/// reads their personal library.
///
/// # Errors
///
/// Returns Subsonic error 70 when the request names a music folder that does
/// not exist, and a generic error when a setting cannot be read.
pub(crate) async fn resolve(call: &Call) -> Result<Scope, SubsonicError> {
    match call.get("musicFolderId") {
        Some(value) if value == LIBRARY_FOLDER_ID => Ok(personal(call)),
        Some(value) if value == SERVER_FOLDER_ID => Ok(server(call.principal().is_admin)),
        Some("") | None => default_scope(call).await,
        Some(value) => {
            tracing::debug!(
                music_folder_id = value,
                "a browse request named a music folder that does not exist"
            );
            Err(SubsonicError::not_found())
        }
    }
}

/// Resolves the scopes a search reads, in the order their matches are listed.
///
/// A search with a `musicFolderId` reads only that folder's scope. A search
/// without one reads the personal library first and then, for an administrator,
/// the server, so stored content the user has not adopted is still found.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// One or two scopes.
///
/// # Errors
///
/// Answers Subsonic error 70 for an unknown `musicFolderId`.
pub(crate) async fn search_scopes(call: &Call) -> Result<Vec<Scope>, SubsonicError> {
    match call.get("musicFolderId") {
        Some("") | None => {
            if call.principal().is_admin {
                Ok(vec![personal(call), server(true)])
            } else {
                Ok(vec![personal(call)])
            }
        }
        Some(_) => Ok(vec![resolve(call).await?]),
    }
}

/// Returns the principal's personal library.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The personal scope of the principal's account.
fn personal(call: &Call) -> Scope {
    Scope::Personal {
        user_id: call.principal().user_id.uuid(),
    }
}

/// Returns the server-wide view a principal may read.
///
/// # Parameters
///
/// - `is_admin`: whether the principal administers the server.
///
/// # Returns
///
/// The server scope, which holds every stored track for an administrator and is
/// [`ServerBrowse::Disabled`] otherwise, which every query answers with an empty
/// result.
fn server(is_admin: bool) -> Scope {
    Scope::server_for_principal(is_admin)
}

/// Resolves the scope a request without `musicFolderId` reads.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The server-wide view when the user's `subsonic.default_scope` override, or
/// the server-wide value of that setting when the user has none, is `server`
/// and the user administers the server; the personal library otherwise.
///
/// # Errors
///
/// Returns a generic error when the override or the setting cannot be read.
async fn default_scope(call: &Call) -> Result<Scope, SubsonicError> {
    // The server view is an administrator's, so the others never resolve to it.
    if !call.principal().is_admin {
        return Ok(personal(call));
    }
    let snapshot = call.state().settings_snapshot();
    let user_id = call.principal().user_id.uuid();
    let configured = match call
        .state()
        .user_settings()
        .get_typed(user_id, keys::SUBSONIC_DEFAULT_SCOPE)
        .await
    {
        Ok(Some(value)) => value,
        Ok(None) => stored_scope(&snapshot)?,
        Err(error) => {
            // A failed override read must not fail the browse: the server-wide
            // value is a usable answer, and the cause is logged.
            tracing::warn!(
                user = %user_id,
                error = %error,
                "could not read the user's default browse scope"
            );
            stored_scope(&snapshot)?
        }
    };

    if configured == SERVER_SCOPE {
        Ok(server(true))
    } else {
        Ok(personal(call))
    }
}

/// Reads the server-wide `subsonic.default_scope` value.
///
/// # Parameters
///
/// - `snapshot`: the settings in effect.
///
/// # Returns
///
/// The configured value, which the registry restricts to `library` or `server`.
///
/// # Errors
///
/// Returns a generic error when the setting cannot be read.
fn stored_scope(snapshot: &SettingsStore) -> Result<String, SubsonicError> {
    snapshot
        .get(keys::SUBSONIC_DEFAULT_SCOPE)
        .map_err(|error| browse::failure("subsonic.default_scope", &error))
}
