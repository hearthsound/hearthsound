//! Integration tests for the browse endpoints: one snapshot pair per endpoint
//! over the seeded library, the scope rules the two virtual music folders
//! imply, and the error an unusable id is answered with.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use hearthsound_config::{SettingValue, keys};
use hearthsound_core::{EntityId, IdPrefix};

use common::seed::{Seeded, library};
use common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library and the account's id.
///
/// # Returns
///
/// The fixture, the ids of the seeded entities, and nothing else; the fixture
/// keeps its temporary directory alive.
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

#[tokio::test]
async fn get_artists_answers_the_index_in_both_encodings() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getArtists?musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains(&seeded.cardinals.to_string()));
    assert!(json.body.contains(&seeded.mello.to_string()));
    assert!(
        !json.body.contains(&seeded.charlotte.to_string()),
        "the evictable cache is outside the default server view: {}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("artists-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getArtists.view?musicFolderId=2&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(
        xml.body
            .contains("ignoredArticles=\"The El La Los Las Le Les\"")
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("artists-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn get_artist_answers_the_artist_with_its_albums() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getArtist?id={}&musicFolderId=2&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains(&seeded.alpha.to_string()));
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("artist-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getArtist.view?id={}&musicFolderId=2&apiKey={}",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("artist-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn get_album_answers_the_album_with_its_songs() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&musicFolderId=1&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(
        json.body
            .contains(&format!("\"songCount\":{}", seeded.alpha_tracks.len())),
        "{}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getAlbum.view?id={}&musicFolderId=1&apiKey={}",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn cover_art_names_the_album_only_when_the_album_has_art() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // The owned folder's files carry embedded art, so the album and its songs
    // all name the album.
    let with_art = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&musicFolderId=1&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&with_art);
    assert_eq!(
        with_art
            .body
            .matches(&format!("\"coverArt\":\"{}\"", seeded.alpha))
            .count(),
        3,
        "the album and both its songs name the album: {}",
        with_art.body
    );

    // The generic folder's file carries no art, so the album's cover art is
    // empty and its song names itself.
    let without_art = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&musicFolderId=2&apiKey={}&f=json",
            seeded.bright, fixture.api_key
        ),
    )
    .await;
    assert_ok(&without_art);
    assert!(
        without_art.body.contains("\"coverArt\":\"\""),
        "{}",
        without_art.body
    );
    assert!(
        without_art
            .body
            .contains(&format!("\"coverArt\":\"{}\"", seeded.bright_track)),
        "{}",
        without_art.body
    );
}

#[tokio::test]
async fn get_song_answers_one_child() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getSong?id={}&musicFolderId=2&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"mediaType\":\"song\""));
    assert!(json.body.contains("\"samplingRate\":44100"));
    assert!(json.body.contains("\"channelCount\":2"));
    assert!(json.body.contains("\"duration\":180"));
    assert!(json.body.contains("\"isrc\":[\"US-AAA-01-00001\"]"));
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("song-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getSong.view?id={}&musicFolderId=2&apiKey={}",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("song-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn get_album_list_2_answers_newest_first() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=newest&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(
        !json.body.contains(&seeded.cache_album.to_string()),
        "an unpinned cache file is outside the default server view: {}",
        json.body
    );
    let delta = json.body.find(&seeded.delta.to_string()).unwrap();
    let alpha = json.body.find(&seeded.alpha.to_string()).unwrap();
    assert!(delta < alpha, "newest first: {}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-list-newest-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getAlbumList2.view?type=newest&musicFolderId=2&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-list-newest-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn get_album_list_2_sorts_alphabetically_and_pages() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=alphabeticalByName&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    let alpha = json.body.find(&seeded.alpha.to_string()).unwrap();
    let bright = json.body.find(&seeded.bright.to_string()).unwrap();
    let delta = json.body.find(&seeded.delta.to_string()).unwrap();
    assert!(alpha < bright && bright < delta, "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-list-alphabetical-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getAlbumList2.view?type=alphabeticalByName&musicFolderId=2&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-list-alphabetical-xml", seeded.stabilize(&xml.body));
    });

    // `size` and `offset` page through the same order.
    let page = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=alphabeticalByName&size=1&offset=1&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&page);
    assert!(
        page.body.contains(&seeded.bright.to_string()),
        "{}",
        page.body
    );
    assert!(
        !page.body.contains(&seeded.alpha.to_string()),
        "{}",
        page.body
    );
    assert_eq!(
        page.body.matches("\"songCount\"").count(),
        1,
        "{}",
        page.body
    );

    // A size of zero returns no albums rather than an error.
    let empty = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=alphabeticalByName&size=0&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&empty);
    assert!(
        empty.body.contains("\"albumList2\":{\"album\":[]}"),
        "{}",
        empty.body
    );
}

#[tokio::test]
async fn get_album_list_2_returns_a_random_album() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // The personal library holds one album, so the random sample is that album
    // whichever way the database orders it.
    let json = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=random&musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains(&seeded.alpha.to_string()));
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-list-random-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getAlbumList2.view?type=random&musicFolderId=1&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-list-random-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn get_album_list_2_rejects_an_unsupported_type() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=frequent&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 0);
    assert!(probe.body.contains("Unsupported album list type."));
}

#[tokio::test]
async fn get_album_list_2_requires_a_type() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!("/rest/getAlbumList2?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&probe, 10);
}

#[tokio::test]
async fn get_album_list_2_rejects_a_non_numeric_size() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=newest&size=lots&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 0);
}

#[tokio::test]
async fn the_owned_folder_is_visible_in_the_personal_library() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let personal = get(
        &app,
        &format!(
            "/rest/getArtists?musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&personal);
    assert!(personal.body.contains(&seeded.cardinals.to_string()));
    assert!(
        !personal.body.contains(&seeded.mello.to_string()),
        "a generic folder is not part of the personal library: {}",
        personal.body
    );
}

#[tokio::test]
async fn a_generic_folder_is_visible_only_in_the_server_scope() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let server = get(
        &app,
        &format!(
            "/rest/getArtists?musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&server);
    assert!(server.body.contains(&seeded.mello.to_string()));

    let personal = get(
        &app,
        &format!(
            "/rest/getArtists?musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&personal);
    assert!(!personal.body.contains(&seeded.mello.to_string()));
}

#[tokio::test]
async fn a_cached_file_is_hidden_while_the_server_view_is_permanent_only() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let permanent_only = get(
        &app,
        &format!(
            "/rest/getArtists?musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&permanent_only);
    assert!(!permanent_only.body.contains(&seeded.charlotte.to_string()));

    fixture
        .settings
        .set(
            keys::PRIVACY_SERVER_BROWSE.name(),
            SettingValue::String("all".to_owned()),
            None,
        )
        .await
        .unwrap();

    let everything = get(
        &app,
        &format!(
            "/rest/getArtists?musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&everything);
    assert!(everything.body.contains(&seeded.charlotte.to_string()));

    let albums = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=alphabeticalByName&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&albums);
    assert!(
        albums.body.contains(&seeded.cache_album.to_string()),
        "{}",
        albums.body
    );
}

#[tokio::test]
async fn a_disabled_server_view_answers_empty() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    fixture
        .settings
        .set(
            keys::PRIVACY_SERVER_BROWSE.name(),
            SettingValue::String("disabled".to_owned()),
            None,
        )
        .await
        .unwrap();

    let artists = get(
        &app,
        &format!(
            "/rest/getArtists?musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&artists);
    assert!(artists.body.contains("\"index\":[]"), "{}", artists.body);
    assert!(!artists.body.contains(&seeded.cardinals.to_string()));

    let albums = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=newest&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&albums);
    assert!(
        albums.body.contains("\"albumList2\":{\"album\":[]}"),
        "{}",
        albums.body
    );
}

#[tokio::test]
async fn the_default_scope_follows_the_user_setting() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // The server-wide default is `library`, so a request without a folder reads
    // the personal library.
    let personal = get(
        &app,
        &format!("/rest/getArtists?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&personal);
    assert!(personal.body.contains(&seeded.cardinals.to_string()));
    assert!(!personal.body.contains(&seeded.mello.to_string()));

    // The user's override wins over the server-wide value.
    fixture
        .state
        .user_settings()
        .set(
            fixture.user_id.uuid(),
            keys::SUBSONIC_DEFAULT_SCOPE.name(),
            SettingValue::String("server".to_owned()),
        )
        .await
        .unwrap();

    let server = get(
        &app,
        &format!("/rest/getArtists?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&server);
    assert!(server.body.contains(&seeded.mello.to_string()));
}

#[tokio::test]
async fn an_unknown_music_folder_answers_error_70() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/getArtists?musicFolderId=3&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn a_missing_id_answers_error_10() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();

    for endpoint in ["getArtist", "getAlbum", "getSong"] {
        let probe = get(
            &app,
            &format!("/rest/{endpoint}?apiKey={}&f=json", fixture.api_key),
        )
        .await;
        assert_error(&probe, 10);
    }
}

#[tokio::test]
async fn a_malformed_id_answers_error_70() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();

    for id in ["nope", "tr_", "tr_ZZZZZZZZZZZZZZZZZZZZZZZZZZ"] {
        let probe = get(
            &app,
            &format!("/rest/getSong?id={id}&apiKey={}&f=json", fixture.api_key),
        )
        .await;
        assert_error(&probe, 70);
    }
}

#[tokio::test]
async fn an_unknown_entity_answers_error_70() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();
    let unknown = EntityId::new(IdPrefix::Track);

    let probe = get(
        &app,
        &format!(
            "/rest/getSong?id={unknown}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn a_catalog_id_answers_error_70() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/getSong?id=x_deezer_t_6crk6d9n6mv0&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn an_id_of_another_kind_answers_error_70() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/getSong?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn an_entity_outside_the_scope_answers_error_70() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // The generic folder's track is not part of the personal library.
    let probe = get(
        &app,
        &format!(
            "/rest/getSong?id={}&musicFolderId=1&apiKey={}&f=json",
            seeded.bright_track, fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);

    let visible = get(
        &app,
        &format!(
            "/rest/getSong?id={}&musicFolderId=2&apiKey={}&f=json",
            seeded.bright_track, fixture.api_key
        ),
    )
    .await;
    assert_ok(&visible);
    assert!(visible.body.contains(&seeded.bright_track.to_string()));
}
