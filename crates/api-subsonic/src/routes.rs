//! The `/rest` router and the endpoint registration helper.
//!
//! Every endpoint is registered at both `/rest/<name>` and `/rest/<name>.view`,
//! for GET and for POST with an `application/x-www-form-urlencoded` body. The
//! two paths reach the same handler, because the `.view` suffix is what older
//! clients append and the base protocol defines the two as equivalent.
//!
//! Anything under `/rest/` that no endpoint matched, including the endpoint
//! families this server never implements (podcast, internet radio, share,
//! video, and chat), is answered with Subsonic error 70 rather than a stub or a
//! bare HTTP 404, so a client is never told a feature it cannot have exists.

use axum::Router;
use axum::handler::Handler;
use axum::routing::get;

use crate::endpoints;
use crate::params::Params;
use crate::response::{Encoded, Response, SubsonicError};
use crate::state::SubsonicState;

/// The path that answers every `/rest` request no endpoint matched.
///
/// This is a route rather than a router fallback so that it survives the router
/// being merged into a larger application.
const NOT_FOUND_PATH: &str = "/rest/{*rest}";

/// Registers an endpoint at `/rest/<name>` and `/rest/<name>.view`.
///
/// Both paths accept GET and POST, so a client can pass the arguments in the
/// query string or in an `application/x-www-form-urlencoded` body (the
/// `formPost` extension). The handler is cloned for each of the four
/// registrations, which is free for a function item.
///
/// # Parameters
///
/// - `router`: the router to add the endpoint to.
/// - `name`: the endpoint name, without a leading slash and without the `.view`
///   suffix.
/// - `handler`: the endpoint's handler.
///
/// # Returns
///
/// The router, with the endpoint registered.
///
/// # Examples
///
/// ```
/// use axum::Router;
/// use hearthsound_api_subsonic::{Call, Encoded, Response, SubsonicState, register};
///
/// async fn pong(call: Call) -> Encoded {
///     call.encode(Response::ok())
/// }
///
/// let router: Router<SubsonicState> = register(Router::new(), "ping", pong);
/// # let _ = router;
/// ```
pub fn register<H, T>(
    router: Router<SubsonicState>,
    name: &str,
    handler: H,
) -> Router<SubsonicState>
where
    H: Handler<T, SubsonicState>,
    T: 'static,
{
    let route = format!("/rest/{name}");
    let view = format!("/rest/{name}.view");
    router
        .route(&route, get(handler.clone()).post(handler.clone()))
        .route(&view, get(handler.clone()).post(handler))
}

/// Builds every route of the `/rest` surface, without its state.
///
/// Register new endpoints here, with [`register`], before the catch-all route.
///
/// # Returns
///
/// The router, which the composition root mounts with
/// [`router`](crate::router).
pub fn routes() -> Router<SubsonicState> {
    let router = Router::new();
    let router = register(router, "ping", endpoints::ping);
    let router = register(router, "getLicense", endpoints::get_license);
    let router = register(
        router,
        "getOpenSubsonicExtensions",
        endpoints::get_open_subsonic_extensions,
    );
    let router = register(router, "tokenInfo", endpoints::token_info);
    let router = register(router, "getMusicFolders", endpoints::get_music_folders);
    let router = register(router, "jukeboxControl", endpoints::jukebox_control);
    let router = register(router, "getArtists", endpoints::get_artists);
    let router = register(
        router,
        "getTranscodeDecision",
        endpoints::get_transcode_decision,
    );
    let router = register(
        router,
        "getTranscodeStream",
        endpoints::get_transcode_stream,
    );
    let router = register(router, "getArtistInfo", endpoints::get_artist_info);
    let router = register(router, "getArtistInfo2", endpoints::get_artist_info_2);
    let router = register(router, "getAlbumInfo", endpoints::get_album_info);
    let router = register(router, "getAlbumInfo2", endpoints::get_album_info_2);
    let router = register(router, "getTopSongs", endpoints::get_top_songs);
    let router = register(router, "getSimilarSongs", endpoints::get_similar_songs);
    let router = register(router, "getSimilarSongs2", endpoints::get_similar_songs_2);
    let router = register(router, "getUser", endpoints::get_user);
    let router = register(router, "getUsers", endpoints::get_users);
    // Account writes belong to the native API and the command line, so the
    // Subsonic calls answer with the documentation link instead.
    let router = register(router, "createUser", endpoints::unsupported_management);
    let router = register(router, "updateUser", endpoints::unsupported_management);
    let router = register(router, "deleteUser", endpoints::unsupported_management);
    let router = register(
        router,
        "changePassword",
        endpoints::unsupported_change_password,
    );
    let router = register(router, "download", endpoints::download);
    let router = register(router, "getScanStatus", endpoints::get_scan_status);
    let router = register(router, "startScan", endpoints::start_scan);
    let router = register(router, "getNowPlaying", endpoints::get_now_playing);
    let router = register(router, "reportPlayback", endpoints::report_playback);
    let router = register(router, "createBookmark", endpoints::create_bookmark);
    let router = register(router, "deleteBookmark", endpoints::delete_bookmark);
    let router = register(router, "getBookmarks", endpoints::get_bookmarks);
    let router = register(router, "savePlayQueue", endpoints::save_play_queue);
    let router = register(router, "getPlayQueue", endpoints::get_play_queue);
    let router = register(
        router,
        "savePlayQueueByIndex",
        endpoints::save_play_queue_by_index,
    );
    let router = register(
        router,
        "getPlayQueueByIndex",
        endpoints::get_play_queue_by_index,
    );
    let router = register(router, "getPlaylists", endpoints::get_playlists);
    let router = register(router, "getPlaylist", endpoints::get_playlist);
    let router = register(router, "createPlaylist", endpoints::create_playlist);
    let router = register(router, "updatePlaylist", endpoints::update_playlist);
    let router = register(router, "deletePlaylist", endpoints::delete_playlist);
    let router = register(router, "setRating", endpoints::set_rating);
    let router = register(router, "star", endpoints::star);
    let router = register(router, "unstar", endpoints::unstar);
    let router = register(router, "getStarred", endpoints::get_starred);
    let router = register(router, "getStarred2", endpoints::get_starred2);
    let router = register(router, "getGenres", endpoints::get_genres);
    let router = register(router, "getSongsByGenre", endpoints::get_songs_by_genre);
    let router = register(router, "getRandomSongs", endpoints::get_random_songs);
    let router = register(router, "getIndexes", endpoints::get_indexes);
    let router = register(router, "getMusicDirectory", endpoints::get_music_directory);
    let router = register(router, "getArtist", endpoints::get_artist);
    let router = register(router, "getAlbum", endpoints::get_album);
    let router = register(router, "getSong", endpoints::get_song);
    let router = register(router, "getAlbumList", endpoints::get_album_list);
    let router = register(router, "getAlbumList2", endpoints::get_album_list_2);
    let router = register(router, "search2", endpoints::search2);
    let router = register(router, "search3", endpoints::search3);
    let router = register(router, "stream", endpoints::stream);
    let router = register(router, "getCoverArt", endpoints::get_cover_art);
    let router = register(router, "scrobble", endpoints::scrobble);
    let router = register(router, "hsGetAvailability", endpoints::hs_get_availability);
    let router = register(router, "hsAcquire", endpoints::hs_acquire);
    let router = register(router, "hsGetAcquisitions", endpoints::hs_get_acquisitions);
    let router = register(
        router,
        "hsCancelAcquisition",
        endpoints::hs_cancel_acquisition,
    );
    let router = register(router, "hsSetQueue", endpoints::hs_set_queue);
    let router = register(router, "hsSave", endpoints::hs_save);
    let router = register(router, "hsUnsave", endpoints::hs_unsave);
    let router = register(router, "hsSetFeedback", endpoints::hs_set_feedback);
    let router = register(
        router,
        "hsSetPlaylistLifetime",
        endpoints::hs_set_playlist_lifetime,
    );
    let router = register(router, "hsGetServerInfo", endpoints::hs_get_server_info);
    let router = register(
        router,
        "hsGetRecommendedAlbums",
        endpoints::hs_get_recommended_albums,
    );
    let router = register(
        router,
        "hsGetSimilarArtistAlbums",
        endpoints::hs_get_similar_artist_albums,
    );
    let router = register(
        router,
        "hsGetLibraryCounts",
        endpoints::hs_get_library_counts,
    );
    router.route(NOT_FOUND_PATH, get(not_found).post(not_found))
}

/// Builds the `/rest` router with its state.
///
/// # Parameters
///
/// - `state`: the shared services every endpoint uses.
///
/// # Returns
///
/// The router, serving every path under `/rest/`.
pub fn router(state: SubsonicState) -> Router {
    routes().with_state(state)
}

/// Answers an endpoint this server does not implement.
///
/// The handler takes no credentials: there is nothing to authorize, and a
/// client that asks for a feature this server has not got is told so whether or
/// not it authenticated.
async fn not_found(params: Params) -> Encoded {
    params.encode(Response::failed(SubsonicError::not_found()))
}
