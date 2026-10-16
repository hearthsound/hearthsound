//! Integration tests for the browse endpoints: one snapshot pair per endpoint
//! over the seeded library, the scope rules the two virtual music folders
//! imply, and the error an unusable id is answered with.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_config::{SettingValue, keys};
use hearthsound_core::{EntityId, IdPrefix};

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

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
async fn cover_art_always_names_the_album_and_its_songs_name_it() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // An album and each of its songs name the album, whether or not a file
    // carries a picture: the endpoint that serves the image answers 404 when
    // nothing resolves, and a client that never asks is never told.
    for (album, folder, songs) in [(seeded.alpha, "1", 2), (seeded.bright, "2", 1)] {
        let listing = get(
            &app,
            &format!(
                "/rest/getAlbum?id={album}&musicFolderId={folder}&apiKey={}&f=json",
                fixture.api_key
            ),
        )
        .await;
        assert_ok(&listing);
        assert_eq!(
            listing
                .body
                .matches(&format!("\"coverArt\":\"{album}\""))
                .count(),
            songs + 1,
            "the album and each of its songs name the album: {}",
            listing.body
        );
    }
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
async fn get_album_list_2_rejects_an_unknown_type() {
    let (fixture, _) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=sideways&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 0);
    assert!(probe.body.contains("Unsupported album list type."));
}

/// The album list orders that read the user's own activity.
#[tokio::test]
async fn get_album_list_2_orders_by_the_users_activity() {
    use hearthsound_core::EntityKind;

    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let user = fixture.user_id.uuid();
    let activity = fixture.state.activity();

    // The user rated and played Delta most, then Alpha, and liked Bright.
    activity
        .set_rating(user, EntityKind::Album, seeded.delta.uuid(), 5)
        .await
        .unwrap();
    activity
        .set_rating(user, EntityKind::Album, seeded.alpha.uuid(), 2)
        .await
        .unwrap();
    activity
        .plays()
        .record_play(
            user,
            seeded.alpha_tracks[0].uuid(),
            Some("dsub"),
            1_000,
            180_000,
            true,
        )
        .await
        .unwrap();
    activity
        .plays()
        .record_play(
            user,
            seeded.delta_track.uuid(),
            Some("dsub"),
            2_000,
            300_000,
            true,
        )
        .await
        .unwrap();
    activity
        .plays()
        .record_play(
            user,
            seeded.delta_track.uuid(),
            Some("dsub"),
            3_000,
            300_000,
            true,
        )
        .await
        .unwrap();
    activity
        .star(user, &[(EntityKind::Album, seeded.bright.uuid())])
        .await
        .unwrap();

    let listed = |list_type: &str| {
        let app = app.clone();
        let api_key = fixture.api_key.clone();
        let list_type = list_type.to_owned();
        async move {
            get(
                &app,
                &format!(
                    "/rest/getAlbumList2?type={list_type}&musicFolderId=2&apiKey={api_key}&f=json"
                ),
            )
            .await
            .body
        }
    };

    // Each order is checked by the position of the albums it moves.
    for (list_type, first, second) in [
        ("highest", seeded.delta, seeded.alpha),
        ("frequent", seeded.delta, seeded.alpha),
        ("recent", seeded.delta, seeded.alpha),
    ] {
        let body = listed(list_type).await;
        let first_at = body
            .find(&first.to_string())
            .expect("the first album is listed");
        let second_at = body
            .find(&second.to_string())
            .expect("the second album is listed");
        assert!(first_at < second_at, "{list_type}: {body}");
        assert!(
            body.contains("\"userRating\""),
            "{list_type} reports the rating: {body}"
        );
    }

    // The liked album comes first in the starred order, with the unliked ones
    // after it in name order.
    let body = listed("starred").await;
    let bright = body.find(&seeded.bright.to_string()).unwrap();
    let alpha = body.find(&seeded.alpha.to_string()).unwrap();
    let delta = body.find(&seeded.delta.to_string()).unwrap();
    assert!(bright < alpha && alpha < delta, "starred: {body}");
    assert!(body.contains("\"starred\""), "starred: {body}");

    // Every album of another order carries no rating of its own except where
    // the user set one: Alpha's is 2 and Delta's is 5.
    let body = listed("alphabeticalByName").await;
    assert!(body.contains("\"userRating\":5"), "{body}");
    assert!(body.contains("\"userRating\":2"), "{body}");
}

/// The year and genre filters, and the parameters they need.
#[tokio::test]
async fn get_album_list_2_filters_by_year_and_genre() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let order = |query: &str| {
        let app = app.clone();
        let api_key = fixture.api_key.clone();
        let query = query.to_owned();
        async move {
            let response = get(
                &app,
                &format!("/rest/getAlbumList2?{query}&musicFolderId=2&apiKey={api_key}&f=json"),
            )
            .await;
            response.body
        }
    };

    // Delta is from 2005, Bright from 2001, Alpha from 1999.
    let body = order("type=byYear&fromYear=2001&toYear=2005").await;
    let bright = body.find(&seeded.bright.to_string()).unwrap();
    let delta = body.find(&seeded.delta.to_string()).unwrap();
    assert!(
        bright < delta,
        "a range that counts up is oldest first: {body}"
    );
    assert!(
        !body.contains(&seeded.alpha.to_string()),
        "1999 is out: {body}"
    );

    let body = order("type=byYear&fromYear=2005&toYear=2001").await;
    let bright = body.find(&seeded.bright.to_string()).unwrap();
    let delta = body.find(&seeded.delta.to_string()).unwrap();
    assert!(
        delta < bright,
        "a range that counts down is newest first: {body}"
    );

    // One end of the range alone lists that year.
    let body = order("type=byYear&fromYear=2001").await;
    assert!(body.contains(&seeded.bright.to_string()), "{body}");
    assert!(!body.contains(&seeded.delta.to_string()), "{body}");

    let body = order("type=byGenre&genre=rock").await;
    assert!(body.contains(&seeded.alpha.to_string()), "{body}");
    assert!(!body.contains(&seeded.bright.to_string()), "{body}");
    let body = order("type=byGenre&genre=ROCK").await;
    assert!(
        body.contains(&seeded.alpha.to_string()),
        "the genre is matched folded: {body}"
    );

    let missing = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=byYear&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&missing, 10);
    let missing = get(
        &app,
        &format!(
            "/rest/getAlbumList2?type=byGenre&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&missing, 10);
}

/// `getAlbumList` answers the same albums as folders a client browses into.
#[tokio::test]
async fn get_album_list_answers_albums_as_folders() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getAlbumList?type=alphabeticalByName&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"albumList\""), "{}", json.body);
    assert!(json.body.contains("\"isDir\":true"), "{}", json.body);
    assert!(json.body.contains("\"songCount\":1"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-list-dir-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getAlbumList.view?type=alphabeticalByName&musicFolderId=2&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-list-dir-xml", seeded.stabilize(&xml.body));
    });
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

/// A listed entity reports the user's own stars, rating, and play counts.
#[tokio::test]
async fn a_listed_entity_reports_the_users_annotation() {
    use hearthsound_core::EntityKind;

    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let user = fixture.user_id.uuid();
    let track = seeded.alpha_tracks[0];
    let activity = fixture.state.activity();

    // The user liked and played the first song, and rated its album.
    activity
        .star(user, &[(EntityKind::Track, track.uuid())])
        .await
        .unwrap();
    activity
        .set_rating(user, EntityKind::Album, seeded.alpha.uuid(), 4)
        .await
        .unwrap();
    activity
        .plays()
        .record_play(
            user,
            track.uuid(),
            Some("dsub"),
            1_700_000_000_000,
            180_000,
            true,
        )
        .await
        .unwrap();

    let song = get(
        &app,
        &format!(
            "/rest/getSong?id={track}&musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&song);
    assert!(
        !song.body.contains("\"userRating\":4"),
        "the song is not the rated album: {}",
        song.body
    );
    assert!(
        song.body.contains("\"playCount\":1"),
        "the play count is the user's own: {}",
        song.body
    );
    assert!(
        song.body.contains("\"played\":\"2023-11-14T22:13:20Z\""),
        "the last play is the user's own: {}",
        song.body
    );
    assert!(
        song.body.contains("\"starred\":\"20"),
        "the like is reported as an instant: {}",
        song.body
    );

    let album = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&musicFolderId=1&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&album);
    assert!(
        album.body.contains("\"userRating\":4"),
        "the album carries the rating: {}",
        album.body
    );
    assert!(
        album.body.contains("\"playCount\":1"),
        "the album sums its tracks' plays: {}",
        album.body
    );

    let artists = get(
        &app,
        &format!("/rest/getArtists?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&artists);
    assert!(
        artists
            .body
            .contains(&format!("\"coverArt\":\"{}\"", seeded.cardinals)),
        "an artist names itself as its cover art: {}",
        artists.body
    );
}
