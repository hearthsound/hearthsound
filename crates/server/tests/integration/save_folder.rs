//! The per-account save location over HTTP: which folders may be named, and
//! which folders the picker is built from.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use hearthsound_auth::{ApiKeys, NewUser, Users};
use hearthsound_core::{Availability, EntityId, IdPrefix, StorageClass};
use hearthsound_library::{
    ArtistCredit, LibraryFolder, LibraryRepository, ManagedMediaFile, Origin, TrackInput,
};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::Fixture;
use crate::native::{assert_problem, bearer, json};

/// Formats a folder row id as the client id the API uses.
///
/// # Parameters
///
/// - `id`: the folder's row id.
fn folder_id(id: Uuid) -> String {
    EntityId::from_uuid(IdPrefix::LibraryFolder, id).to_string()
}

/// Finds one entry of a settings listing.
///
/// # Parameters
///
/// - `listing`: the listing the endpoint answered.
/// - `key`: the dotted setting key to find.
fn setting<'a>(listing: &'a Value, key: &str) -> &'a Value {
    listing["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["key"] == key)
        .unwrap_or_else(|| panic!("{key} is listed: {listing}"))
}

/// Adds a folder the test's account or another one owns.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `name`: the directory's name beside the data tree.
/// - `owner`: the account that owns it, or `None` for a generic folder.
async fn folder(fixture: &Fixture, name: &str, owner: Option<Uuid>) -> LibraryFolder {
    let path = fixture.library_dir(name);
    std::fs::create_dir_all(&path).unwrap();
    LibraryRepository::new(fixture.services.database())
        .add_library_folder(&path, owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_save_folder_must_be_an_owned_folder() {
    let fixture = Fixture::new().await;
    let other = Users::new(fixture.services.database())
        .create(
            &fixture.services.settings().current(),
            &NewUser::new("other", "Other", "hunter22", false),
        )
        .await
        .unwrap();
    let own = folder(&fixture, "music-own", Some(fixture.user_id)).await;
    let theirs = folder(&fixture, "music-other", Some(other.id)).await;
    let own_id = folder_id(own.id);

    let refused = bearer(
        &fixture,
        Method::PATCH,
        "/api/v1/me/settings",
        Some(json!({ "settings": { "library.save_folder": folder_id(theirs.id) } })),
    )
    .await;
    assert_problem(refused, StatusCode::BAD_REQUEST, "invalid-request").await;

    let unknown = EntityId::from_uuid(IdPrefix::LibraryFolder, Uuid::now_v7()).to_string();
    let refused = bearer(
        &fixture,
        Method::PATCH,
        "/api/v1/me/settings",
        Some(json!({ "settings": { "library.save_folder": unknown } })),
    )
    .await;
    assert_problem(refused, StatusCode::BAD_REQUEST, "invalid-request").await;

    let patched = bearer(
        &fixture,
        Method::PATCH,
        "/api/v1/me/settings",
        Some(json!({ "settings": { "library.save_folder": own_id } })),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::OK);
    let patched = json(patched).await;
    let saved = setting(&patched, "library.save_folder");
    assert_eq!(saved["value"], own_id);
    assert_eq!(saved["source"], "user");
    assert_eq!(
        saved["serverValue"], "",
        "the managed library is the server value"
    );

    let cleared = json(
        bearer(
            &fixture,
            Method::PATCH,
            "/api/v1/me/settings",
            Some(json!({ "settings": { "library.save_folder": null } })),
        )
        .await,
    )
    .await;
    let saved = setting(&cleared, "library.save_folder");
    assert_eq!(saved["source"], "server");
    assert_eq!(saved["value"], "");

    let server_wide = bearer(
        &fixture,
        Method::PATCH,
        "/api/v1/admin/settings",
        Some(json!({ "settings": { "library.save_folder": own_id } })),
    )
    .await;
    assert_problem(server_wide, StatusCode::BAD_REQUEST, "invalid-request").await;
    fixture.stop().await;
}

#[tokio::test]
async fn the_folder_picker_lists_only_the_callers_folders() {
    let fixture = Fixture::new().await;
    let other = Users::new(fixture.services.database())
        .create(
            &fixture.services.settings().current(),
            &NewUser::new("other", "Other", "hunter22", false),
        )
        .await
        .unwrap();
    let own = folder(&fixture, "my-music", Some(fixture.user_id)).await;
    let theirs = folder(&fixture, "their-music", Some(other.id)).await;
    let generic = folder(&fixture, "everyone-music", None).await;

    let listing = json(bearer(&fixture, Method::GET, "/api/v1/me/folders", None).await).await;
    let folders = listing["folders"].as_array().unwrap();
    assert_eq!(
        folders.len(),
        1,
        "only the caller's folder is listed: {listing}"
    );
    assert_eq!(folders[0]["id"], folder_id(own.id));
    assert_eq!(folders[0]["name"], "my-music");
    assert_eq!(folders[0]["path"], own.path.to_string_lossy().as_ref());
    for unlisted in [theirs.id, generic.id] {
        assert!(
            folders
                .iter()
                .all(|entry| entry["id"] != folder_id(unlisted)),
            "a folder the caller does not own is never offered"
        );
    }
    fixture.stop().await;
}

/// Points a user's save location at a folder.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `user`: the account whose save location changes.
/// - `folder`: the folder it names.
async fn point_at(fixture: &Fixture, user: Uuid, folder: &LibraryFolder) {
    sqlx::query(
        "INSERT INTO user_settings (user_id, key, value) VALUES (?, 'library.save_folder', ?) \
         ON CONFLICT (user_id, key) DO UPDATE SET value = excluded.value",
    )
    .bind(user.as_bytes().as_slice())
    .bind(serde_json::Value::String(folder_id(folder.id)).to_string())
    .execute(fixture.services.database().writer())
    .await
    .unwrap();
}

/// Stores a track with one real audio file in the cache root and returns it.
///
/// The file is generated, so a later scan of the folder the track is saved into
/// can probe it and the rescan assertion is about rows rather than about a file
/// nothing can read.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `rel_path`: the path under the cache root the file is stored at.
async fn saved_track(fixture: &Fixture, rel_path: &str) -> EntityId {
    let directory = fixture
        .services
        .storage_roots()
        .root_for(StorageClass::Cache)
        .join(Path::new(rel_path).parent().unwrap());
    std::fs::create_dir_all(&directory).unwrap();
    let audio = AudioFixture::new(AudioFormat::Flac)
        .seconds(0.2)
        .file_name(
            Path::new(rel_path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap(),
        )
        .tags(TrackTags::new("Hello Song", "Hello Artist").album("Hello Song album"))
        .write_into(&directory)
        .await
        .unwrap();
    let metadata = std::fs::metadata(&audio).unwrap();
    let library = LibraryRepository::new(fixture.services.database());
    let mut input = TrackInput::new("Hello Song", Origin::Catalog, Availability::Cached);
    input.display_artist = Some("Hello Artist".to_owned());
    let track = library
        .insert_track(&input, &[ArtistCredit::new("Hello Artist", "main")], &[])
        .await
        .unwrap();
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Cache,
            rel_path: rel_path.to_owned(),
            size: i64::try_from(metadata.len()).unwrap(),
            mtime_ms: 1,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            duration_ms: None,
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: None,
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    track
}

/// Likes a track over `/rest` and asserts the like was recorded.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `track`: the track to like.
async fn star(fixture: &Fixture, track: EntityId) {
    let response = fixture
        .get(&format!(
            "/rest/star?id={track}&apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("\"status\":\"ok\""));
}

/// The audio files one directory holds, recursively.
///
/// # Parameters
///
/// - `root`: the directory to walk.
fn audio_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension == "flac")
            {
                found.push(path);
            }
        }
    }
    found
}

/// The `media_files` rows a track has, as `(storage, folder_id, rel_path)`.
async fn rows(fixture: &Fixture, track: EntityId) -> Vec<(String, Option<Vec<u8>>, String)> {
    sqlx::query_as("SELECT storage, folder_id, rel_path FROM media_files WHERE track_id = ?")
        .bind(track.uuid().as_bytes().to_vec())
        .fetch_all(fixture.services.database().readers())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_liked_track_moves_into_the_owners_folder() {
    let fixture = Fixture::new().await;
    let folder = folder(&fixture, "music", Some(fixture.user_id)).await;
    point_at(&fixture, fixture.user_id, &folder).await;
    let rel_path = "Hello Artist/Hello Song album/01 - Hello Song.flac";
    let track = saved_track(&fixture, rel_path).await;

    star(&fixture, track).await;

    let rows = rows(&fixture, track).await;
    assert_eq!(rows.len(), 1, "one file, and so one row");
    assert_eq!(rows[0].0, "folder");
    assert_eq!(rows[0].1.as_deref(), Some(folder.id.as_bytes().as_slice()));
    assert!(
        folder.path.join(&rows[0].2).is_file(),
        "the file is in the owner's folder"
    );
    assert!(
        !fixture
            .services
            .storage_roots()
            .root_for(StorageClass::Cache)
            .join(rel_path)
            .exists(),
        "the managed copy is gone"
    );
    let starred = fixture
        .get(&format!(
            "/rest/getStarred2?apiKey={}&f=json",
            fixture.api_key
        ))
        .await
        .text()
        .await
        .unwrap();
    assert!(
        starred.contains(&track.to_string()),
        "the like survived the move: {starred}"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn a_rescan_does_not_duplicate_the_saved_track() {
    let fixture = Fixture::new().await;
    let folder = folder(&fixture, "music", Some(fixture.user_id)).await;
    point_at(&fixture, fixture.user_id, &folder).await;
    let track = saved_track(
        &fixture,
        "Hello Artist/Hello Song album/01 - Hello Song.flac",
    )
    .await;
    star(&fixture, track).await;
    let saved: String = rows(&fixture, track).await[0].2.clone();

    fixture
        .services
        .scanner()
        .scan_folder(folder.id)
        .await
        .unwrap();

    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM tracks WHERE title = ?")
        .bind("Hello Song")
        .fetch_all(fixture.services.database().readers())
        .await
        .unwrap();
    assert_eq!(titles.len(), 1, "the scan finds one track, not two");
    assert_eq!(rows(&fixture, track).await.len(), 1, "one media row");
    assert!(
        folder.path.join(saved).is_file(),
        "the saved file is still the one the row names"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn two_accounts_liking_one_track_each_get_their_own_file() {
    let fixture = Fixture::new().await;
    let other = Users::new(fixture.services.database())
        .create(
            &fixture.services.settings().current(),
            &NewUser::new("other", "Other", "hunter22", false),
        )
        .await
        .unwrap();
    let other_key = ApiKeys::new(fixture.services.database())
        .generate(other.id, "test")
        .await
        .unwrap()
        .plaintext;
    let mine = folder(&fixture, "music-a", Some(fixture.user_id)).await;
    let theirs = folder(&fixture, "music-b", Some(other.id)).await;
    point_at(&fixture, fixture.user_id, &mine).await;
    point_at(&fixture, other.id, &theirs).await;
    let track = saved_track(
        &fixture,
        "Hello Artist/Hello Song album/01 - Hello Song.flac",
    )
    .await;

    let first_path = format!("/rest/star?id={track}&apiKey={}&f=json", fixture.api_key);
    let second_path = format!("/rest/star?id={track}&apiKey={other_key}&f=json");
    let (first, second) = tokio::join!(fixture.get(&first_path), fixture.get(&second_path),);
    for response in [first, second] {
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.text().await.unwrap().contains("\"status\":\"ok\""));
    }

    // The first saver's like moves the managed file into their folder; the
    // second saver's like then finds the only file in someone else's folder and
    // copies it, so each account's folder is self-contained and neither file is
    // lost (decision 20).
    let rows = rows(&fixture, track).await;
    assert_eq!(rows.len(), 2, "one row per saver's folder");
    assert_ne!(rows[0].1, rows[1].1, "each row names its own folder");
    assert!(
        rows.iter().all(|row| row.0 == "folder"),
        "no managed copy is left: {rows:?}"
    );
    assert_eq!(audio_files(&mine.path).len(), 1);
    assert_eq!(audio_files(&theirs.path).len(), 1);
    for key in [&fixture.api_key, &other_key] {
        let starred = fixture
            .get(&format!("/rest/getStarred2?apiKey={key}&f=json"))
            .await
            .text()
            .await
            .unwrap();
        assert!(
            starred.contains(&track.to_string()),
            "both likes are recorded: {starred}"
        );
    }
    fixture.stop().await;
}

#[tokio::test]
async fn a_save_never_overwrites_a_file_that_is_already_there() {
    let fixture = Fixture::new().await;
    let folder = folder(&fixture, "music", Some(fixture.user_id)).await;
    point_at(&fixture, fixture.user_id, &folder).await;
    // The first like reveals the name the template renders for these tags.
    let first = saved_track(
        &fixture,
        "Hello Artist/Hello Song album/01 - Hello Song.flac",
    )
    .await;
    star(&fixture, first).await;
    let saved: String = rows(&fixture, first).await[0].2.clone();
    // Clear the folder back to a file the server did not write.
    sqlx::query("DELETE FROM media_files WHERE track_id = ?")
        .bind(first.uuid().as_bytes().to_vec())
        .execute(fixture.services.database().writer())
        .await
        .unwrap();
    let occupied = folder.path.join(&saved);
    std::fs::write(&occupied, b"a file the save must not replace").unwrap();
    let decoy = std::fs::read(&occupied).unwrap();

    let second = saved_track(
        &fixture,
        "Hello Artist/Hello Song album/01 - Hello Song.flac",
    )
    .await;
    star(&fixture, second).await;

    assert_eq!(
        std::fs::read(&occupied).unwrap(),
        decoy,
        "the file that held the name is untouched"
    );
    let rows = rows(&fixture, second).await;
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].2, saved, "the save takes the next collision suffix");
    assert!(folder.path.join(&rows[0].2).is_file());
    fixture.stop().await;
}

#[tokio::test]
async fn unsaving_keeps_the_saved_file() {
    let fixture = Fixture::new().await;
    let folder = folder(&fixture, "music", Some(fixture.user_id)).await;
    point_at(&fixture, fixture.user_id, &folder).await;
    let track = saved_track(
        &fixture,
        "Hello Artist/Hello Song album/01 - Hello Song.flac",
    )
    .await;
    star(&fixture, track).await;
    let saved: String = rows(&fixture, track).await[0].2.clone();

    let response = fixture
        .get(&format!(
            "/rest/unstar?id={track}&apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let evicted = bearer(&fixture, Method::POST, "/api/v1/admin/storage/evict", None).await;
    assert_eq!(evicted.status(), StatusCode::OK);

    assert!(
        folder.path.join(&saved).is_file(),
        "unsaving leaves the file where it is"
    );
    let rows = rows(&fixture, track).await;
    assert_eq!(rows.len(), 1, "the folder row survives");
    assert_eq!(rows[0].0, "folder");
    fixture.stop().await;
}

#[tokio::test]
async fn a_second_account_gets_a_copy_of_a_file_in_someone_elses_folder() {
    let fixture = Fixture::new().await;
    let other = Users::new(fixture.services.database())
        .create(
            &fixture.services.settings().current(),
            &NewUser::new("other", "Other", "hunter22", false),
        )
        .await
        .unwrap();
    let other_key = ApiKeys::new(fixture.services.database())
        .generate(other.id, "test")
        .await
        .unwrap()
        .plaintext;
    let mine = folder(&fixture, "music-a", Some(fixture.user_id)).await;
    let theirs = folder(&fixture, "music-b", Some(other.id)).await;
    point_at(&fixture, fixture.user_id, &mine).await;
    point_at(&fixture, other.id, &theirs).await;
    let track = saved_track(
        &fixture,
        "Hello Artist/Hello Song album/01 - Hello Song.flac",
    )
    .await;

    star(&fixture, track).await;
    let mine_files = audio_files(&mine.path);
    assert_eq!(mine_files.len(), 1);
    let first_bytes = std::fs::read(&mine_files[0]).unwrap();

    let response = fixture
        .get(&format!("/rest/star?id={track}&apiKey={other_key}&f=json"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        std::fs::read(&mine_files[0]).unwrap(),
        first_bytes,
        "the first saver's file is untouched"
    );
    let theirs_files = audio_files(&theirs.path);
    assert_eq!(
        theirs_files.len(),
        1,
        "the only file was in another account's folder, so it is copied"
    );
    assert_eq!(
        rows(&fixture, track).await.len(),
        2,
        "each folder has its own row"
    );
    let stream = fixture
        .get(&format!(
            "/rest/stream?id={track}&apiKey={other_key}&f=json"
        ))
        .await;
    assert_eq!(stream.status(), StatusCode::OK);
    assert!(
        !stream.bytes().await.unwrap().is_empty(),
        "the track still streams for the second account"
    );
    fixture.stop().await;
}
