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
