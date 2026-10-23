//! Full-stack tests of the playlist editing routes: the reorder that rewrites
//! every position in one request and the problems a bad request answers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::{Availability, EntityId};
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin, TrackInput};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};

use crate::common::Fixture;
use crate::multi_user::create_user;
use crate::native::json as read_json;
use crate::on_demand::{catalog_album_id, install_catalog, materialize};

/// Inserts an album with `count` tracks and returns the track ids in order.
///
/// # Parameters
///
/// - `library`: the repository to write through.
/// - `count`: how many tracks the album holds.
///
/// # Returns
///
/// The track ids, in track-number order.
async fn seed_tracks(library: &LibraryRepository, count: usize) -> Vec<EntityId> {
    let mut album = AlbumInput::new("Reorder Album", Origin::Local);
    album.display_artist = Some("Reorder Artist".to_owned());
    let album_id = library
        .upsert_album(&album, &[ArtistCredit::new("Reorder Artist", "main")])
        .await
        .unwrap();
    let mut tracks = Vec::with_capacity(count);
    for index in 0..count {
        let mut input = TrackInput::new(
            format!("Reorder Song {index}"),
            Origin::Local,
            Availability::Local,
        );
        input.album_id = Some(album_id);
        input.display_artist = Some("Reorder Artist".to_owned());
        input.track_number = Some(index as i64 + 1);
        input.duration_ms = Some(120_000);
        tracks.push(
            library
                .insert_track(&input, &[ArtistCredit::new("Reorder Artist", "main")], &[])
                .await
                .unwrap(),
        );
    }
    tracks
}

/// Creates a playlist that holds `tracks` and returns its client id.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `tracks`: the tracks the playlist holds, in order.
async fn create_playlist(fixture: &Fixture, tracks: &[EntityId]) -> String {
    let playlist = fixture
        .services
        .activity()
        .playlists()
        .create(fixture.user_id, "Reorder Mix", None, false, tracks)
        .await
        .unwrap();
    playlist.id.to_string()
}

/// Sends a reorder and returns the response.
async fn reorder(
    fixture: &Fixture,
    api_key: &str,
    playlist: &str,
    order: &[u32],
) -> reqwest::Response {
    fixture
        .client
        .put(format!(
            "{}/api/v1/playlists/{playlist}/order",
            fixture.base_url()
        ))
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(CONTENT_TYPE, "application/json")
        .body(json!({ "order": order }).to_string())
        .send()
        .await
        .unwrap()
}

/// Reads a playlist through the Subsonic surface and returns its song ids.
async fn subsonic_order(fixture: &Fixture, playlist: &str) -> Vec<String> {
    let response = fixture
        .get(&format!(
            "/rest/getPlaylist?id={playlist}&apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = read_json(response).await;
    body["subsonic-response"]["playlist"]["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_owned())
        .collect()
}

/// A reorder rewrites every position in one request, and the playlist reads
/// back in the new order through the Subsonic surface.
#[tokio::test]
async fn a_reorder_rewrites_the_order_of_a_playlist() {
    let fixture = Fixture::new().await;
    let tracks = seed_tracks(&LibraryRepository::new(fixture.services.database()), 3).await;
    let playlist = create_playlist(&fixture, &tracks).await;

    let response = reorder(&fixture, &fixture.api_key, &playlist, &[2, 0, 1]).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let ids: Vec<String> = tracks.iter().map(ToString::to_string).collect();
    assert_eq!(
        subsonic_order(&fixture, &playlist).await,
        [ids[2].clone(), ids[0].clone(), ids[1].clone()]
    );
    fixture.stop().await;
}

/// A list that is not a permutation of the playlist's positions is a 400
/// `invalid-order`, and the playlist keeps its order.
#[tokio::test]
async fn a_reorder_that_is_not_a_permutation_is_refused() {
    let fixture = Fixture::new().await;
    let tracks = seed_tracks(&LibraryRepository::new(fixture.services.database()), 3).await;
    let playlist = create_playlist(&fixture, &tracks).await;

    for bad in [vec![0, 0, 1], vec![0, 1], vec![0, 1, 3]] {
        let response = reorder(&fixture, &fixture.api_key, &playlist, &bad).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = read_json(response).await;
        assert_eq!(body["code"], "invalid-order", "{body}");
    }

    let ids: Vec<String> = tracks.iter().map(ToString::to_string).collect();
    assert_eq!(
        subsonic_order(&fixture, &playlist).await,
        [ids[0].clone(), ids[1].clone(), ids[2].clone()],
        "a refused reorder writes nothing"
    );
    fixture.stop().await;
}

/// Another account cannot reorder a playlist it does not own: the playlist is
/// answered as not found, exactly like an edit through `updatePlaylist`.
#[tokio::test]
async fn another_account_cannot_reorder_a_playlist() {
    let fixture = Fixture::new().await;
    let tracks = seed_tracks(&LibraryRepository::new(fixture.services.database()), 3).await;
    let playlist = create_playlist(&fixture, &tracks).await;
    let (_, other_key) = create_user(&fixture, "reorderer", false).await;

    let response = reorder(&fixture, &other_key, &playlist, &[2, 0, 1]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = read_json(response).await;
    assert_eq!(body["code"], "not-found", "{body}");

    let ids: Vec<String> = tracks.iter().map(ToString::to_string).collect();
    assert_eq!(
        subsonic_order(&fixture, &playlist).await,
        [ids[0].clone(), ids[1].clone(), ids[2].clone()]
    );
    fixture.stop().await;
}

/// Saves a track to the test account's personal library.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `track`: the track to save.
async fn save(fixture: &Fixture, track: EntityId) {
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'saved', 0)",
    )
    .bind(fixture.user_id.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(fixture.services.database().writer())
    .await
    .unwrap();
}

/// Sends an add-to-playlist request and returns the response.
async fn add_tracks(
    fixture: &Fixture,
    api_key: &str,
    playlist: &str,
    source: &str,
) -> reqwest::Response {
    fixture
        .client
        .post(format!(
            "{}/api/v1/playlists/{playlist}/tracks",
            fixture.base_url()
        ))
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header(CONTENT_TYPE, "application/json")
        .body(json!({ "source": source }).to_string())
        .send()
        .await
        .unwrap()
}

/// A catalog album is materialized and appended with its whole tracklist.
#[tokio::test]
async fn adding_a_catalog_album_materializes_it() {
    let fixture = Fixture::new().await;
    install_catalog(&fixture.services).await;
    fixture.services.registry().rebuild().await.unwrap();
    let album = catalog_album_id(&fixture).await;
    let playlist = create_playlist(&fixture, &[]).await;

    let response = add_tracks(
        &fixture,
        &fixture.api_key,
        &playlist,
        &format!("album:{album}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = read_json(response).await;
    assert_eq!(body["added"], 5, "{body}");
    assert_eq!(body["trackCount"], 5, "{body}");
    assert_eq!(subsonic_order(&fixture, &playlist).await.len(), 5);
    fixture.stop().await;
}

/// A catalog album that is already partly stored is completed: the album
/// source appends the provider's whole tracklist, not only the stored tracks.
#[tokio::test]
async fn adding_a_partially_stored_catalog_album_adds_every_track() {
    let fixture = Fixture::new().await;
    install_catalog(&fixture.services).await;
    fixture.services.registry().rebuild().await.unwrap();
    let album = catalog_album_id(&fixture).await;

    // Two of the album's five tracks are already stored, the way a search
    // result or a suggested track materializes them on their own.
    let first = materialize(fixture.services.catalog(), 1).await.to_string();
    let third = materialize(fixture.services.catalog(), 3).await.to_string();

    let playlist = create_playlist(&fixture, &[]).await;
    let response = add_tracks(
        &fixture,
        &fixture.api_key,
        &playlist,
        &format!("album:{album}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = read_json(response).await;
    assert_eq!(body["added"], 5, "{body}");
    assert_eq!(body["trackCount"], 5, "{body}");

    let response = fixture
        .get(&format!(
            "/rest/getPlaylist?id={playlist}&apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    let body: Value = read_json(response).await;
    let entries = body["subsonic-response"]["playlist"]["entry"]
        .as_array()
        .unwrap();
    let ids: Vec<&str> = entries
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 5);
    assert_eq!(ids[0], first, "the stored track keeps its row");
    assert_eq!(
        ids[2], third,
        "the stored track keeps its place in the album"
    );
    let titles: Vec<&str> = entries
        .iter()
        .map(|entry| entry["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        [
            "Fixture Song 1",
            "Fixture Song 2",
            "Fixture Song 3",
            "Fixture Song 4",
            "Fixture Song 5",
        ],
        "the playlist holds the provider's whole tracklist in order"
    );
    fixture.stop().await;
}

/// An album source appends every track of the album in album order.
#[tokio::test]
async fn adding_an_album_appends_every_track_in_album_order() {
    let fixture = Fixture::new().await;
    let library = LibraryRepository::new(fixture.services.database());
    let tracks = seed_tracks(&library, 3).await;
    let album = library
        .get_track(tracks[0])
        .await
        .unwrap()
        .unwrap()
        .album_id;
    let album = album.unwrap();
    // The queue reads an album through the personal scope, and a saved track
    // is what puts the album there.
    save(&fixture, tracks[0]).await;
    let playlist = create_playlist(&fixture, &[]).await;

    let response = add_tracks(
        &fixture,
        &fixture.api_key,
        &playlist,
        &format!("album:{album}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = read_json(response).await;
    assert_eq!(body["added"], 3, "{body}");
    assert_eq!(body["trackCount"], 3, "{body}");

    let ids: Vec<String> = tracks.iter().map(ToString::to_string).collect();
    assert_eq!(
        subsonic_order(&fixture, &playlist).await,
        [ids[0].clone(), ids[1].clone(), ids[2].clone()]
    );
    fixture.stop().await;
}

/// A playlist source appends its tracks in the playlist's own order.
#[tokio::test]
async fn adding_a_playlist_appends_its_tracks_in_order() {
    let fixture = Fixture::new().await;
    let tracks = seed_tracks(&LibraryRepository::new(fixture.services.database()), 3).await;
    let source = create_playlist(&fixture, &[tracks[2], tracks[0]]).await;
    let target = create_playlist(&fixture, &[]).await;

    let response = add_tracks(
        &fixture,
        &fixture.api_key,
        &target,
        &format!("playlist:{source}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = read_json(response).await;
    assert_eq!(body["added"], 2, "{body}");
    assert_eq!(body["trackCount"], 2, "{body}");

    assert_eq!(
        subsonic_order(&fixture, &target).await,
        [tracks[2].to_string(), tracks[0].to_string()]
    );
    fixture.stop().await;
}

/// An artist source appends the artist's tracks from the personal scope.
#[tokio::test]
async fn adding_an_artist_appends_its_tracks() {
    let fixture = Fixture::new().await;
    let library = LibraryRepository::new(fixture.services.database());
    let tracks = seed_tracks(&library, 3).await;
    let album = library
        .get_track(tracks[0])
        .await
        .unwrap()
        .unwrap()
        .album_id;
    let artists = library.list_album_artists(album.unwrap()).await.unwrap();
    let artist = artists[0].id;
    for track in &tracks {
        save(&fixture, *track).await;
    }
    let playlist = create_playlist(&fixture, &[]).await;

    let response = add_tracks(
        &fixture,
        &fixture.api_key,
        &playlist,
        &format!("artist:{artist}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = read_json(response).await;
    assert_eq!(body["added"], 3, "{body}");
    assert_eq!(body["trackCount"], 3, "{body}");
    assert_eq!(subsonic_order(&fixture, &playlist).await.len(), 3);
    fixture.stop().await;
}

/// `songs`, `liked`, `uploads` and `artist-top:` are not sources this route
/// accepts, and an unknown collection is a 404.
#[tokio::test]
async fn adding_a_source_this_route_does_not_accept_is_refused() {
    let fixture = Fixture::new().await;
    let tracks = seed_tracks(&LibraryRepository::new(fixture.services.database()), 1).await;
    let playlist = create_playlist(&fixture, &[]).await;

    let library = LibraryRepository::new(fixture.services.database());
    let album = library
        .get_track(tracks[0])
        .await
        .unwrap()
        .unwrap()
        .album_id;
    let artist = library.list_album_artists(album.unwrap()).await.unwrap()[0].id;
    for source in [
        "songs".to_owned(),
        "liked".to_owned(),
        "uploads".to_owned(),
        format!("artist-top:{artist}"),
    ] {
        let response = add_tracks(&fixture, &fixture.api_key, &playlist, &source).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{source}");
        let body: Value = read_json(response).await;
        assert_eq!(body["code"], "unknown-source", "{source}: {body}");
    }

    let missing = add_tracks(
        &fixture,
        &fixture.api_key,
        &playlist,
        &format!("album:{}", tracks[0]),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let body: Value = read_json(missing).await;
    assert_eq!(body["code"], "not-found", "{body}");
    fixture.stop().await;
}

/// Another account cannot add tracks to a playlist it does not own.
#[tokio::test]
async fn another_account_cannot_add_tracks_to_a_playlist() {
    let fixture = Fixture::new().await;
    let playlist = create_playlist(&fixture, &[]).await;
    let (_, other_key) = create_user(&fixture, "adder", false).await;

    let response = add_tracks(
        &fixture,
        &other_key,
        &playlist,
        &format!("playlist:{playlist}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value = read_json(response).await;
    assert_eq!(body["code"], "not-found", "{body}");
    assert_eq!(subsonic_order(&fixture, &playlist).await.len(), 0);
    fixture.stop().await;
}
