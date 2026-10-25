//! Integration tests for the playlist endpoints: the round trip a client makes
//! from creating a playlist to deleting it, the entries a catalog song id
//! becomes, the ownership rules, and the cover art a playlist answers with.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use hearthsound_auth::{ApiKeys, NewUser, Users};
use hearthsound_core::EntityId;
use tower::ServiceExt;

use crate::common::artwork::{Art, seed_album};
use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library.
///
/// # Returns
///
/// The fixture and the ids of the seeded entities; the fixture keeps its
/// temporary directory alive.
async fn fixture() -> (Fixture, Seeded) {
    let fixture = Fixture::new().await;
    let seeded = library(
        &fixture.database,
        &fixture.subdir("owned"),
        &fixture.subdir("generic"),
        fixture.user_id,
    )
    .await;
    (fixture, seeded)
}

/// Creates an account with the given name and admin flag.
///
/// # Parameters
///
/// - `fixture`: the fixture whose database the account is written to.
/// - `username`: the login name.
/// - `is_admin`: whether the account administers the server.
///
/// # Returns
///
/// The account's client id and one of its API keys.
async fn account(fixture: &Fixture, username: &str, is_admin: bool) -> (EntityId, String) {
    let user = Users::new(&fixture.database)
        .create(
            &fixture.settings.current(),
            &NewUser::new(username, username, "hunter22", is_admin),
        )
        .await
        .unwrap();
    let key = ApiKeys::new(&fixture.database)
        .generate(user.id, username)
        .await
        .unwrap()
        .plaintext;
    (
        EntityId::from_uuid(hearthsound_core::IdPrefix::User, user.id),
        key,
    )
}

/// Returns the id of the playlist a response carries.
///
/// # Parameters
///
/// - `body`: the response body.
///
/// # Returns
///
/// The id, or `None` when the body does not carry one.
fn playlist_id(body: &str) -> Option<String> {
    let start = body.find("\"id\":\"pl_")?;
    let rest = &body[start + "\"id\":\"".len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// A playlist object carries its lifetime when the vendor extension is asked
/// for, and the standard response stays the standard one without it.
#[tokio::test]
async fn the_playlist_lifetime_rides_the_vendor_extension() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let key = fixture.api_key.clone();

    let created = get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Evening&songId={}&apiKey={key}&f=json",
            seeded.alpha_tracks[0]
        ),
    )
    .await;
    assert_ok(&created);
    let id = playlist_id(&created.body).expect("the response carries the playlist id");

    let mut filters = SNAPSHOT_FILTERS.to_vec();
    filters.push((r#"pl_[0-9a-z]+"#, "pl_ID"));

    // Without the flag neither the listing nor the playlist carries a member.
    let standard = get(&app, &format!("/rest/getPlaylists?apiKey={key}&f=json")).await;
    assert_ok(&standard);
    assert!(
        !standard.body.contains("\"hearthsound\":{"),
        "a standard response has no vendor member: {}",
        standard.body
    );

    // A permanent playlist reports its lifetime as permanent.
    let permanent = get(
        &app,
        &format!("/rest/getPlaylist?id={id}&apiKey={key}&hearthsound=1&f=json"),
    )
    .await;
    assert_ok(&permanent);
    assert!(
        permanent.body.contains("\"temporary\":false"),
        "{}",
        permanent.body
    );
    insta::with_settings!({filters => filters.clone()}, {
        insta::assert_snapshot!(
            "playlist-lifetime-permanent-json",
            seeded.stabilize(&permanent.body)
        );
    });

    // The same playlist made temporary reports its expiry.
    let made = get(
        &app,
        &format!(
            "/rest/hsSetPlaylistLifetime?apiKey={key}&hearthsound=1&f=json&playlistId={id}&temporary=true&expiresAt=4000"
        ),
    )
    .await;
    assert_ok(&made);
    let temporary = get(
        &app,
        &format!("/rest/getPlaylist?id={id}&apiKey={key}&hearthsound=1&f=json"),
    )
    .await;
    assert_ok(&temporary);
    assert!(
        temporary.body.contains("\"temporary\":true"),
        "{}",
        temporary.body
    );
    assert!(
        temporary.body.contains("\"expiresAt\":4000"),
        "{}",
        temporary.body
    );
    insta::with_settings!({filters => filters.clone()}, {
        insta::assert_snapshot!(
            "playlist-lifetime-temporary-json",
            seeded.stabilize(&temporary.body)
        );
    });

    // The listing carries the member of every playlist it lists, in both
    // encodings.
    let listed = get(
        &app,
        &format!("/rest/getPlaylists?apiKey={key}&hearthsound=1&f=json"),
    )
    .await;
    assert_ok(&listed);
    assert!(listed.body.contains("\"hearthsound\":{"), "{}", listed.body);
    insta::with_settings!({filters => filters.clone()}, {
        insta::assert_snapshot!(
            "playlist-lifetime-listing-json",
            seeded.stabilize(&listed.body)
        );
    });
    let xml = get(
        &app,
        &format!("/rest/getPlaylists?apiKey={key}&hearthsound=1"),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("<hearthsound"), "{}", xml.body);
    insta::with_settings!({filters => filters}, {
        insta::assert_snapshot!("playlist-lifetime-listing-xml", seeded.stabilize(&xml.body));
    });
}

/// `createPlaylist` stores a playlist whose entries `getPlaylist` lists.
#[tokio::test]
async fn a_playlist_round_trip() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let created = get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Evening&songId={}&songId={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], seeded.alpha_tracks[1], fixture.api_key
        ),
    )
    .await;
    assert_ok(&created);
    assert!(
        created.body.contains("\"name\":\"Evening\""),
        "{}",
        created.body
    );
    assert!(
        created.body.contains("\"owner\":\"echo\""),
        "the playlist names its owner: {}",
        created.body
    );
    assert!(created.body.contains("\"songCount\":2"), "{}", created.body);
    // A playlist id is minted per playlist, so the snapshots hold a marker for
    // it.
    let mut filters = SNAPSHOT_FILTERS.to_vec();
    filters.push((r#"pl_[0-9a-z]+"#, "pl_ID"));
    insta::with_settings!({filters => filters.clone()}, {
        insta::assert_snapshot!("created-playlist-json", seeded.stabilize(&created.body));
    });
    let id = playlist_id(&created.body).expect("the response carries the playlist id");

    let listed = get(
        &app,
        &format!("/rest/getPlaylists?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"name\":\"Evening\""),
        "{}",
        listed.body
    );
    assert!(
        listed.body.contains(&format!("\"id\":\"{id}\"")),
        "{}",
        listed.body
    );
    assert!(
        !listed.body.contains("\"entry\":[{"),
        "a listing carries no entries: {}",
        listed.body
    );

    let playlist = get(
        &app,
        &format!(
            "/rest/getPlaylist?id={id}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&playlist);
    assert!(
        playlist
            .body
            .contains(&format!("\"id\":\"{}\"", seeded.alpha_tracks[0])),
        "the entries are the songs it was created with: {}",
        playlist.body
    );
    assert!(playlist.body.contains("\"entry\":[{"), "{}", playlist.body);
    insta::with_settings!({filters => filters.clone()}, {
        insta::assert_snapshot!("playlist-json", seeded.stabilize(&playlist.body));
    });

    let xml = get(
        &app,
        &format!("/rest/getPlaylist.view?id={id}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => filters}, {
        insta::assert_snapshot!("playlist-xml", seeded.stabilize(&xml.body));
    });

    // The name, the comment, and the tracks change, and an index removes one.
    let updated = get(
        &app,
        &format!(
            "/rest/updatePlaylist?playlistId={id}&name=Night&comment=quiet&public=true&songIdToAdd={}&songIndexToRemove=0&apiKey={}&f=json",
            seeded.bright_track, fixture.api_key
        ),
    )
    .await;
    assert_ok(&updated);
    let after = get(
        &app,
        &format!(
            "/rest/getPlaylist?id={id}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&after);
    assert!(after.body.contains("\"name\":\"Night\""), "{}", after.body);
    assert!(
        after.body.contains("\"comment\":\"quiet\""),
        "{}",
        after.body
    );
    assert!(after.body.contains("\"public\":true"), "{}", after.body);
    assert!(
        !after
            .body
            .contains(&format!("\"id\":\"{}\"", seeded.alpha_tracks[0])),
        "the removed entry is gone: {}",
        after.body
    );
    assert!(
        after
            .body
            .contains(&format!("\"id\":\"{}\"", seeded.bright_track)),
        "the added entry is there: {}",
        after.body
    );

    let deleted = get(
        &app,
        &format!(
            "/rest/deletePlaylist?id={id}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&deleted);
    let gone = get(
        &app,
        &format!(
            "/rest/getPlaylist?id={id}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&gone, 70);
}

/// `createPlaylist` with a playlist id replaces the tracks of an owned one.
#[tokio::test]
async fn create_playlist_replaces_an_owned_one() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let created = get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Evening&songId={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&created);
    let id = playlist_id(&created.body).expect("the response carries the playlist id");

    let replaced = get(
        &app,
        &format!(
            "/rest/createPlaylist?playlistId={id}&songId={}&apiKey={}&f=json",
            seeded.bright_track, fixture.api_key
        ),
    )
    .await;
    assert_ok(&replaced);
    assert!(
        replaced.body.contains("\"name\":\"Evening\""),
        "the name is kept: {}",
        replaced.body
    );
    assert!(
        replaced.body.contains("\"songCount\":1"),
        "{}",
        replaced.body
    );
    assert!(
        replaced
            .body
            .contains(&format!("\"id\":\"{}\"", seeded.bright_track)),
        "{}",
        replaced.body
    );
    assert!(
        !replaced
            .body
            .contains(&format!("\"id\":\"{}\"", seeded.alpha_tracks[0])),
        "the old tracks are gone: {}",
        replaced.body
    );
}

/// A catalog song id is materialized into a stored entry.
#[tokio::test]
async fn a_catalog_song_id_becomes_a_stored_entry() {
    let (fixture, _seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let found = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&found);
    let start = found
        .body
        .find("x_hello_t_")
        .expect("the search lists a track");
    let rest = &found.body[start..];
    let end = rest.find('"').unwrap();
    let catalog_song = &rest[..end];

    let created = get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Later&songId={catalog_song}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&created);
    assert!(
        created.body.contains("\"songCount\":1"),
        "the catalog song is stored and added: {}",
        created.body
    );
    assert!(
        !created.body.contains("x_hello_t_"),
        "the entry names the stored row: {}",
        created.body
    );
}

/// Only an owner changes a playlist, and only an administrator reads another
/// account's playlists.
#[tokio::test]
async fn playlists_belong_to_their_owner() {
    let (fixture, seeded) = fixture().await;
    let (other, other_key) = account(&fixture, "other", false).await;
    let (admin, admin_key) = account(&fixture, "root", true).await;
    let app = fixture.app();

    let created = get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Evening&songId={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&created);
    let id = playlist_id(&created.body).expect("the response carries the playlist id");

    // Another account may not see a private playlist, and may not change it.
    let hidden = get(
        &app,
        &format!("/rest/getPlaylist?id={id}&apiKey={other_key}&f=json"),
    )
    .await;
    assert_error(&hidden, 70);
    let refused = get(
        &app,
        &format!("/rest/updatePlaylist?playlistId={id}&name=Mine&apiKey={other_key}&f=json"),
    )
    .await;
    assert_error(&refused, 50);
    let refused = get(
        &app,
        &format!("/rest/deletePlaylist?id={id}&apiKey={other_key}&f=json"),
    )
    .await;
    assert_error(&refused, 50);
    let refused = get(
        &app,
        &format!(
            "/rest/createPlaylist?playlistId={id}&songId={}&apiKey={other_key}&f=json",
            seeded.bright_track
        ),
    )
    .await;
    assert_error(&refused, 50);

    // A non-administrator may not ask for another account's playlists.
    let refused = get(
        &app,
        &format!("/rest/getPlaylists?username=echo&apiKey={other_key}&f=json"),
    )
    .await;
    assert_error(&refused, 50);
    let refused = get(
        &app,
        &format!("/rest/getPlaylists?username=nobody&apiKey={admin_key}&f=json"),
    )
    .await;
    assert_error(&refused, 70);

    // An administrator may ask, and sees what the account makes visible: the
    // private playlist is not listed.
    let listed = get(
        &app,
        &format!("/rest/getPlaylists?username=echo&apiKey={admin_key}&f=json"),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"playlist\":[]"),
        "a private playlist stays private: {}",
        listed.body
    );

    // The owner's own name is accepted from anyone.
    let own = get(
        &app,
        &format!("/rest/getPlaylists?username=other&apiKey={other_key}&f=json"),
    )
    .await;
    assert_ok(&own);
    assert!(
        own.body.contains("\"playlist\":[]"),
        "another account's listing is empty: {}",
        own.body
    );

    // A public playlist is listed for every account, and the administrator's
    // ask for its owner lists it too.
    let shared = get(
        &app,
        &format!(
            "/rest/updatePlaylist?playlistId={id}&public=true&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&shared);
    let listed = get(
        &app,
        &format!("/rest/getPlaylists?username=echo&apiKey={admin_key}&f=json"),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"name\":\"Evening\""),
        "a public playlist is listed: {}",
        listed.body
    );
    assert!(
        listed.body.contains("\"owner\":\"echo\""),
        "{}",
        listed.body
    );
    let seen = get(
        &app,
        &format!("/rest/getPlaylist?id={id}&apiKey={other_key}&f=json"),
    )
    .await;
    assert_ok(&seen);
    assert_ne!(other, admin);
}

/// `getCoverArt` answers a playlist with the art of its first entry.
#[tokio::test]
async fn a_playlist_answers_with_the_art_of_its_first_entry() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Alpha",
        &[Art::Embedded {
            size: 64,
            rgb: 0x0033_66ff,
        }],
    )
    .await;
    let app = fixture.app();

    let created = get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Evening&songId={}&apiKey={}&f=json",
            seeded.first_track(),
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&created);
    let id = playlist_id(&created.body).expect("the response carries the playlist id");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/rest/getCoverArt?id={id}&apiKey={}",
                    fixture.api_key
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "image/png",
        "the playlist answers with the entry's image"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        bytes.as_ref(),
        seeded.embedded.as_ref().unwrap().as_slice(),
        "the image is the one the entry's file carries"
    );
}

/// The playlist endpoints refuse the ids and parameters they cannot use.
#[tokio::test]
async fn playlist_requests_are_refused_without_usable_values() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let missing_name = get(
        &app,
        &format!("/rest/createPlaylist?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing_name, 10);
    let missing_id = get(
        &app,
        &format!(
            "/rest/updatePlaylist?name=Night&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&missing_id, 10);
    let missing_id = get(
        &app,
        &format!("/rest/deletePlaylist?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing_id, 10);
    let missing_id = get(
        &app,
        &format!("/rest/getPlaylist?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing_id, 10);

    // An unknown playlist, an id of another kind, and a song id that names no
    // stored song are all refused.
    let unknown = get(
        &app,
        &format!(
            "/rest/getPlaylist?id=pl_01m52y0e7te0htkcfr47tgjnmn&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);
    let other_kind = get(
        &app,
        &format!(
            "/rest/getPlaylist?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&other_kind, 70);
    let bad_song = get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Later&songId={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_song, 70);

    // A bad index and a bad boolean are refused without changing anything.
    let created = get(
        &app,
        &format!(
            "/rest/createPlaylist?name=Evening&songId={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&created);
    let id = playlist_id(&created.body).expect("the response carries the playlist id");
    let bad_index = get(
        &app,
        &format!(
            "/rest/updatePlaylist?playlistId={id}&songIndexToRemove=first&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_index, 0);
    let bad_public = get(
        &app,
        &format!(
            "/rest/updatePlaylist?playlistId={id}&public=maybe&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_public, 0);
}

/// A generated playlist names the service that generated it, and the standard
/// response is unchanged by that member.
#[tokio::test]
async fn a_generated_playlist_names_its_service() {
    let (fixture, seeded) = fixture().await;
    fixture
        .install_discovery_layer(&[("discovery", serde_json::json!({}))])
        .await;
    let instance = fixture.instance_id("discovery");
    let playlists = hearthsound_activity::Playlists::new(&fixture.database);
    let tracks = vec![seeded.alpha_tracks[0], seeded.alpha_tracks[1]];
    let created = playlists
        .upsert_generated(
            &hearthsound_activity::SyncedPlaylist {
                owner_user_id: fixture.user_id.uuid(),
                source_instance_id: instance,
                source_ref: "weekly-jams",
                name: "Weekly Jams",
                comment: Some("A week of listening"),
                expires_at: 4_000,
                tracks: &tracks,
            },
            1_000,
        )
        .await
        .unwrap();
    let app = fixture.app();
    let key = fixture.api_key.clone();

    // Without the flag the playlist is the standard one, with no vendor
    // member at all.
    let standard = get(&app, &format!("/rest/getPlaylists?apiKey={key}&f=json")).await;
    assert_ok(&standard);
    assert!(
        !standard.body.contains("\"hearthsound\":{"),
        "a standard response has no vendor member: {}",
        standard.body
    );

    // With the flag the member names the generating instance.
    let vendor = get(
        &app,
        &format!("/rest/getPlaylists?apiKey={key}&hearthsound=1&f=json"),
    )
    .await;
    assert_ok(&vendor);
    assert!(
        vendor
            .body
            .contains("\"generatedBy\":\"Discovery discovery\""),
        "{}",
        vendor.body
    );
    assert!(
        vendor.body.contains(&format!("\"id\":\"{}\"", created.id)),
        "{}",
        vendor.body
    );

    // The XML encoding carries the same member.
    let xml = get(
        &app,
        &format!("/rest/getPlaylists?apiKey={key}&hearthsound=1"),
    )
    .await;
    assert_ok(&xml);
    assert!(
        xml.body.contains("generatedBy=\"Discovery discovery\""),
        "{}",
        xml.body
    );
}
