//! Full-stack tests for users, personal libraries and storage that no other
//! integration module covers: a second user's access and streams, ad-hoc
//! remote plays into the cache, the delete unpin action, and per-user
//! feedback.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hearthsound_auth::{ApiKeys, Users};
use hearthsound_core::{Availability, EntityId, StorageClass};
use hearthsound_library::LibraryRepository;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::Fixture;
use crate::native::{albums_in, bearer, json as read_json};
use crate::on_demand::{
    FIRST_ISRC, catalog_album_id, catalog_id, downloads_dir, eventual, give_track_isrc,
    install_acquisition, install_catalog, ledger_rows, materialize, place_audio, play_stream, star,
};
use crate::storage::managed_track;

/// How long a test waits for a background import or a held stream to finish.
const PATIENCE: Duration = Duration::from_secs(5);

/// Creates a user through the admin API and returns its id and a bearer key
/// that acts as it.
///
/// # Parameters
///
/// - `fixture`: the running fixture, whose account is an administrator.
/// - `username`: the new account's login name.
/// - `acquire`: whether the account may acquire (and so stream remote tracks).
///
/// # Returns
///
/// The user's id and a plaintext API key for it.
pub(crate) async fn create_user(
    fixture: &Fixture,
    username: &str,
    acquire: bool,
) -> (Uuid, String) {
    let created = bearer(
        fixture,
        Method::POST,
        "/api/v1/admin/users",
        Some(json!({
            "username": username,
            "password": "correct horse",
            "permissions": { "acquire": acquire, "upload": true, "download": true },
        })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let database = fixture.services.database();
    let user = Users::new(database)
        .find_by_username(username)
        .await
        .unwrap()
        .unwrap();
    let key = ApiKeys::new(database)
        .generate(user.id, "multi-user test")
        .await
        .unwrap();
    (user.id, key.plaintext)
}

/// Reads a Subsonic answer and returns its `subsonic-response` member.
async fn subsonic(response: reqwest::Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    read_json(response).await["subsonic-response"].clone()
}

/// Applies server settings through the admin API and expects success.
async fn patch_settings(fixture: &Fixture, settings: Value) {
    let patched = bearer(
        fixture,
        Method::PATCH,
        "/api/v1/admin/settings",
        Some(json!({ "settings": settings })),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::OK);
}

/// Returns the storage classes of a track's media files.
async fn stored_in(fixture: &Fixture, track: EntityId) -> Vec<StorageClass> {
    LibraryRepository::new(fixture.services.database())
        .list_media_files_for_track(track)
        .await
        .unwrap()
        .into_iter()
        .map(|file| file.storage)
        .collect()
}

/// Returns a track's availability as stored.
async fn availability_of(fixture: &Fixture, track: EntityId) -> Availability {
    LibraryRepository::new(fixture.services.database())
        .get_track(track)
        .await
        .unwrap()
        .unwrap()
        .availability
}

/// Counts the dislikes a user has recorded on tracks.
async fn track_dislikes_of(fixture: &Fixture, user: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_feedback \
         WHERE user_id = ? AND entity_kind = 'track' AND value = 'dislike'",
    )
    .bind(user.as_bytes().to_vec())
    .fetch_one(fixture.services.database().readers())
    .await
    .unwrap()
}

/// Lists every regular file under a directory, recursively; a missing
/// directory has none.
///
/// # Parameters
///
/// - `dir`: the directory to walk.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// A user without the acquire permission gets 403 for a remote stream and
/// acquires nothing; a user with it streams the same track (200).
#[tokio::test]
async fn a_user_without_acquire_gets_403_for_a_remote_stream() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    let track = materialize(services.catalog(), 1).await;
    give_track_isrc(&services, track, FIRST_ISRC).await;
    let id = catalog_id(1);

    let (_, listener_key) = create_user(&fixture, "listener", false).await;
    let refused = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={id}&apiKey={listener_key}",
            fixture.base_url()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(ledger_rows(services.orchestrator(), track).await.is_empty());

    play_stream(&fixture, &id).await;
}

/// A liked catalog album is stored once in the library root and shows only in
/// the liker's library; another user streams one of its tracks (200) without
/// it entering their own library.
#[tokio::test]
async fn a_liked_catalog_album_is_streamed_by_others_without_entering_their_library() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;
    let roots = services.storage_roots().clone();

    let album = catalog_album_id(&fixture).await;
    star(&fixture, "albumId", &album).await;

    let mut tracks = Vec::new();
    for number in 1..=5 {
        tracks.push(materialize(services.catalog(), number).await);
    }
    let library = LibraryRepository::new(services.database());
    let mut imported = None;
    for _ in 0..1500 {
        for track in &tracks {
            if let Some(file) = library
                .list_media_files_for_track(*track)
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                imported = Some((*track, file));
            }
        }
        if imported.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (track, file) = imported.expect("the liked album imports one track");
    assert_eq!(file.storage, StorageClass::Library);
    assert!(roots.library.join(&file.rel_path).is_file());

    let (_, listener_key) = create_user(&fixture, "listener", true).await;
    let streamed = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={track}&apiKey={listener_key}",
            fixture.base_url()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(streamed.status(), StatusCode::OK);
    let body = tokio::time::timeout(PATIENCE, streamed.bytes())
        .await
        .expect("the stream ends")
        .unwrap();
    assert_eq!(
        body.as_ref(),
        std::fs::read(roots.library.join(&file.rel_path)).unwrap()
    );

    assert!(
        albums_in(&fixture, &fixture.api_key, 1)
            .await
            .contains(&"Fixture Album".to_owned()),
        "the liker's library holds the album"
    );
    assert!(
        albums_in(&fixture, &listener_key, 1).await.is_empty(),
        "another user's library does not gain the album"
    );
}

/// A remote play by a user who did not like the track lands in the cache, and
/// the file is not in the library root.
#[tokio::test]
async fn an_ad_hoc_remote_play_lands_in_the_cache() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;
    let roots = services.storage_roots().clone();

    let track = materialize(services.catalog(), 1).await;
    give_track_isrc(&services, track, FIRST_ISRC).await;
    let (_, listener_key) = create_user(&fixture, "listener", true).await;

    let response = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={}&apiKey={listener_key}",
            fixture.base_url(),
            catalog_id(1)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::timeout(PATIENCE, response.bytes())
        .await
        .expect("the stream ends")
        .unwrap();

    eventual(|| async { stored_in(&fixture, track).await == [StorageClass::Cache] }).await;
    assert_eq!(availability_of(&fixture, track).await, Availability::Cached);
    assert_eq!(files_under(&roots.cache).len(), 1);
    assert!(files_under(&roots.library).is_empty());
}

/// With `storage.unpin_action=delete`, unstarring a stored track deletes its
/// file from both roots and leaves the track remote.
#[tokio::test]
async fn unstarring_deletes_the_file_when_the_unpin_action_is_delete() {
    let fixture = Fixture::new().await;
    let roots = fixture.services.storage_roots().clone();
    let track = managed_track(&fixture, StorageClass::Library, "Delete/Delete Song.flac").await;
    let id = track.to_string();

    patch_settings(&fixture, json!({ "storage.unpin_action": "delete" })).await;
    star(&fixture, "id", &id).await;
    let unstarred = subsonic(
        fixture
            .get(&format!(
                "/rest/unstar?id={id}&apiKey={}&f=json",
                fixture.api_key
            ))
            .await,
    )
    .await;
    assert_eq!(unstarred["status"], "ok");

    eventual(|| async {
        stored_in(&fixture, track).await.is_empty()
            && availability_of(&fixture, track).await == Availability::Remote
    })
    .await;
    assert!(files_under(&roots.library).is_empty());
    assert!(files_under(&roots.cache).is_empty());
}

/// Ratings, plays, stars and dislikes are per user: another user sees none of
/// the admin's, and a dislike from a rating of one is recorded for the rater
/// only.
#[tokio::test]
async fn feedback_ratings_and_plays_stay_with_their_user() {
    let fixture = Fixture::new().await;
    let track = managed_track(&fixture, StorageClass::Library, "Local/Feedback Song.flac").await;
    let (listener, listener_key) = create_user(&fixture, "listener", true).await;
    let id = track.to_string();
    let admin_key = fixture.api_key.clone();

    star(&fixture, "id", &id).await;
    let rated = subsonic(
        fixture
            .get(&format!(
                "/rest/setRating?id={id}&rating=4&apiKey={admin_key}&f=json"
            ))
            .await,
    )
    .await;
    assert_eq!(rated["status"], "ok");
    let scrobbled = subsonic(
        fixture
            .get(&format!(
                "/rest/scrobble?id={id}&submission=true&apiKey={admin_key}&f=json"
            ))
            .await,
    )
    .await;
    assert_eq!(scrobbled["status"], "ok");

    let mine = subsonic(
        fixture
            .get(&format!("/rest/getSong?id={id}&apiKey={admin_key}&f=json"))
            .await,
    )
    .await;
    assert!(!mine["song"]["starred"].is_null(), "{mine}");
    assert_eq!(mine["song"]["userRating"], 4, "{mine}");
    assert_eq!(mine["song"]["playCount"], 1, "{mine}");

    let listed = subsonic(
        fixture
            .get(&format!("/rest/getStarred2?apiKey={listener_key}&f=json"))
            .await,
    )
    .await;
    assert!(
        listed["starred2"]["song"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{listed}"
    );

    patch_settings(&fixture, json!({ "subsonic.rating_one_is_dislike": true })).await;
    let disliked = subsonic(
        fixture
            .get(&format!(
                "/rest/setRating?id={id}&rating=1&apiKey={listener_key}&f=json"
            ))
            .await,
    )
    .await;
    assert_eq!(disliked["status"], "ok");
    assert_eq!(track_dislikes_of(&fixture, listener).await, 1);
    assert_eq!(track_dislikes_of(&fixture, fixture.user_id).await, 0);
}
