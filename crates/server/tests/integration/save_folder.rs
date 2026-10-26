//! The per-account save location over HTTP: which folders may be named, and
//! which folders the picker is built from.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::StatusCode;
use hearthsound_auth::{NewUser, Users};
use hearthsound_core::{EntityId, IdPrefix};
use hearthsound_library::{LibraryFolder, LibraryRepository};
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
