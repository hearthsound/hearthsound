//! Integration tests for `search3`: the local-first ordering, the catalog
//! results a fixture provider adds, the scope rule, and the empty-query
//! listing.
//!
//! The catalog side is the `hello-catalog` fixture installed through the
//! provider registry, so the catalog results in a snapshot are exactly what a
//! real provider would contribute.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_config::{SettingValue, keys};
use hearthsound_core::Availability;
use hearthsound_library::{ArtistCredit, LibraryRepository, Origin, TrackInput};

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library and the account's id.
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
async fn search3_answers_local_results_then_catalog_results() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    // The catalog track is named `x_hello_t_...` and appended after the local
    // songs.
    assert!(json.body.contains("x_hello_t_"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("local-and-catalog-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/search3.view?query=cardinals&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("local-and-catalog-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn search3_with_an_empty_query_lists_the_local_library() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!("/rest/search3?query=&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    // The empty query lists everything local and never reaches the catalog.
    assert!(
        json.body.contains(&seeded.alpha.to_string()),
        "{}",
        json.body
    );
    assert!(
        !json.body.contains("x_hello_"),
        "an empty query returns no catalog results: {}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("empty-query-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!("/rest/search3.view?query=&apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("empty-query-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn search3_answers_local_results_when_the_catalog_is_disabled() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    fixture
        .settings
        .set(
            keys::SEARCH_CATALOG_ENABLED.name(),
            SettingValue::Bool(false),
            None,
        )
        .await
        .unwrap();
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains(&seeded.alpha.to_string()),
        "{}",
        probe.body
    );
    assert!(
        !probe.body.contains("x_hello_"),
        "a disabled catalog contributes nothing: {}",
        probe.body
    );
}

#[tokio::test]
async fn search3_drops_a_catalog_track_the_server_already_stores() {
    let (fixture, _seeded) = fixture().await;
    fixture.install_hello_catalog().await;

    // A local track the hello catalog also returns: the same title, artist, and
    // duration, so deduplication drops the catalog copy instead of listing it
    // twice.
    let library = LibraryRepository::new(&fixture.database);
    let mut input = TrackInput::new("Hello, cardinals", Origin::Local, Availability::Local);
    input.display_artist = Some("Hello Artist".to_owned());
    input.duration_ms = Some(180_000);
    let local = library
        .insert_track(&input, &[ArtistCredit::new("Hello Artist", "main")], &[])
        .await
        .unwrap();
    let app = fixture.app();

    // The server scope sees the stored track, which no folder owns.
    let probe = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(probe.body.contains(&local.to_string()), "{}", probe.body);
    assert_eq!(
        probe.body.matches("Hello, cardinals").count(),
        1,
        "the stored recording is listed once: {}",
        probe.body
    );
    assert!(
        !probe.body.contains("x_hello_t_"),
        "the catalog track is already stored: {}",
        probe.body
    );
    assert!(
        probe.body.contains("x_hello_a_"),
        "the catalog album is not stored and is appended: {}",
        probe.body
    );
}

#[tokio::test]
async fn search3_requires_a_query() {
    let (fixture, _seeded) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!("/rest/search3?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&probe, 10);
}

/// `search2` answers the same matches as `search3` in the older shape: a bare
/// artist reference, an album folder, and a song.
#[tokio::test]
async fn search2_answers_the_same_matches_in_the_older_shape() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/search2?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"searchResult2\""), "{}", json.body);
    // The artist is a bare reference, the album is a folder, and the catalog
    // track follows the local songs.
    assert!(
        json.body.contains(&format!(
            "{{\"id\":\"{}\",\"name\":\"The Cardinals\"}}",
            seeded.cardinals
        )),
        "the artist carries its id and name: {}",
        json.body
    );
    assert!(json.body.contains("\"isDir\":true"), "{}", json.body);
    assert!(json.body.contains("x_hello_t_"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("search2-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/search2.view?query=cardinals&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("search2-xml", seeded.stabilize(&xml.body));
    });

    let missing = get(
        &app,
        &format!("/rest/search2?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing, 10);
}

/// `search2` reports the user's star on a listed artist, and lists the library
/// for an empty query without reaching the catalog.
#[tokio::test]
async fn search2_reports_stars_and_lists_the_library_for_an_empty_query() {
    let (fixture, seeded) = fixture().await;
    use hearthsound_core::EntityKind;

    fixture.install_hello_catalog().await;
    fixture
        .state
        .activity()
        .star(
            fixture.user_id.uuid(),
            &[(EntityKind::Artist, seeded.cardinals.uuid())],
        )
        .await
        .unwrap();
    let app = fixture.app();

    let starred = get(
        &app,
        &format!(
            "/rest/search2?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&starred);
    assert!(
        starred.body.contains("\"starred\":\""),
        "the listed artist carries the user's star: {}",
        starred.body
    );

    let empty = get(
        &app,
        &format!("/rest/search2?query=&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&empty);
    assert!(
        empty.body.contains(&seeded.alpha.to_string()),
        "{}",
        empty.body
    );
    assert!(
        !empty.body.contains("x_hello_"),
        "an empty query returns no catalog results: {}",
        empty.body
    );
}

/// Without a music folder a search finds stored content outside the personal
/// library after the library's own matches; folder 1 keeps to the library,
/// folder 2 to the server, and a non-administrator reaches the library only.
#[tokio::test]
async fn a_search_without_a_folder_reaches_beyond_the_personal_library() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let mello = seeded.mello.to_string();
    let search = |folder: &str| {
        format!(
            "/rest/search3?query=mello{folder}&apiKey={}&f=json",
            fixture.api_key
        )
    };

    let everywhere = get(&app, &search("")).await;
    assert_ok(&everywhere);
    assert!(everywhere.body.contains(&mello), "{}", everywhere.body);

    let library_only = get(&app, &search("&musicFolderId=1")).await;
    assert_ok(&library_only);
    assert!(!library_only.body.contains(&mello), "{}", library_only.body);

    let server_only = get(&app, &search("&musicFolderId=2")).await;
    assert_ok(&server_only);
    assert!(server_only.body.contains(&mello), "{}", server_only.body);

    // A non-administrator's search reads their personal library only, with or
    // without a folder.
    let (_, limited) = fixture.account(&[]).await;
    for folder in ["", "&musicFolderId=2"] {
        let searched = get(
            &app,
            &format!("/rest/search3?query=mello{folder}&apiKey={limited}&f=json"),
        )
        .await;
        assert_ok(&searched);
        assert!(!searched.body.contains(&mello), "{}", searched.body);
    }
}

/// With `hearthsound=1`, every song, album and artist of `search3` and
/// `search2` carries a relevance score; without the flag the answer is the
/// standard shape and holds no vendor member.
#[tokio::test]
async fn search_results_carry_relevance_scores_when_the_vendor_extension_is_on() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"score\":"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("scored-search3-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/search3.view?query=cardinals&apiKey={}&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("score=\""), "{}", xml.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("scored-search3-xml", seeded.stabilize(&xml.body));
    });

    let search2 = get(
        &app,
        &format!(
            "/rest/search2?query=cardinals&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&search2);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("scored-search2-json", seeded.stabilize(&search2.body));
    });

    let plain = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&plain);
    assert!(
        !plain.body.contains("\"hearthsound\":"),
        "without the flag the answer has no vendor member: {}",
        plain.body
    );
    assert!(
        !plain.body.contains("\"score\":"),
        "without the flag no score is computed into the answer: {}",
        plain.body
    );
}

/// Playing a catalog song materializes its rows; the search that found it must
/// keep answering with them for a listener who saved nothing.
#[tokio::test]
async fn a_played_catalog_entity_stays_in_the_search_that_found_it() {
    use hearthsound_core::{ClientId, EntityKind};

    let fixture = Fixture::new().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();
    let uri = format!(
        "/rest/search3?query=world&apiKey={}&f=json",
        fixture.api_key
    );

    let before = get(&app, &uri).await;
    assert_ok(&before);
    let (artist, album, song) = catalog_ids(&before.body);

    // The play path materializes the track before it can fail or succeed; its
    // album and artist come with it.
    let Ok(ClientId::Catalog(track)) = ClientId::parse(&song) else {
        panic!("the song id is a catalog id: {song}");
    };
    assert_eq!(track.kind(), EntityKind::Track);
    let stored = fixture.catalog.materialize_track(&track).await.unwrap();

    let after = get(&app, &uri).await;
    assert_ok(&after);
    assert!(
        after.body.contains(&song),
        "the song the listener just played must stay listed: {}",
        after.body
    );
    assert!(
        after.body.contains(&album),
        "the album must stay listed: {}",
        after.body
    );
    assert!(
        after.body.contains(&artist),
        "the artist must stay listed: {}",
        after.body
    );

    // Saving it makes the stored rows the only ones that show these entities.
    let saved = get(
        &app,
        &format!(
            "/rest/hsSave?hearthsound=1&apiKey={}&f=json&id={stored}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&saved);
    let listed = get(&app, &uri).await;
    assert_ok(&listed);
    assert!(
        listed.body.contains(&stored.to_string()),
        "the saved song is the local result: {}",
        listed.body
    );
    assert!(
        !listed.body.contains(&song),
        "a saved song is listed once: {}",
        listed.body
    );
}

/// Reads the first catalog artist, album and song ids of a `search3` body.
///
/// # Parameters
///
/// - `body`: the JSON response body.
///
/// # Returns
///
/// The artist, album and song ids, in that order.
fn catalog_ids(body: &str) -> (String, String, String) {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    let result = &value["subsonic-response"]["searchResult3"];
    let id = |kind: &str| {
        result[kind][0]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("the search answered a catalog {kind}: {body}"))
            .to_owned()
    };
    (id("artist"), id("album"), id("song"))
}

/// A play that cannot deliver a file still leaves the catalog arrays of the
/// search that found it exactly as they were.
#[tokio::test]
async fn a_failed_play_leaves_the_catalog_arrays_alone() {
    let mut fixture = Fixture::new().await;
    fixture.install_hello_catalog().await;
    // No acquisition instance exists, so the play is admitted and parks; the
    // fail mode answers the request instead of holding it.
    fixture.add_acquisition_layer().await;
    fixture
        .settings
        .set(
            keys::STREAM_PENDING_MODE.name(),
            SettingValue::String("fail".to_owned()),
            None,
        )
        .await
        .unwrap();
    let app = fixture.app();
    let uri = format!(
        "/rest/search3?query=world&apiKey={}&f=json",
        fixture.api_key
    );

    let before = get(&app, &uri).await;
    assert_ok(&before);
    let artists = catalog_list(&before.body, "artist");
    let albums = catalog_list(&before.body, "album");
    let songs = catalog_list(&before.body, "song");
    assert!(!artists.is_empty() && !albums.is_empty() && !songs.is_empty());

    let play = get(
        &app,
        &format!("/rest/stream?id={}&apiKey={}", songs[0], fixture.api_key),
    )
    .await;
    assert!(
        !play.status.is_success(),
        "the play cannot deliver a file: {}",
        play.status
    );

    let after = get(&app, &uri).await;
    assert_ok(&after);
    assert_eq!(
        catalog_list(&after.body, "artist"),
        artists,
        "the artist array keeps its entries and their provider order: {}",
        after.body
    );
    assert_eq!(
        catalog_list(&after.body, "album"),
        albums,
        "the album array keeps its entries and their provider order: {}",
        after.body
    );
    assert_eq!(catalog_list(&after.body, "song"), songs);
}

/// Reads the ids of one catalog kind of a `search3` body, in its order.
///
/// # Parameters
///
/// - `body`: the JSON response body.
/// - `kind`: the member to read: `artist`, `album` or `song`.
///
/// # Returns
///
/// The ids of the catalog entries of that kind.
fn catalog_list(body: &str, kind: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["subsonic-response"]["searchResult3"][kind]
        .as_array()
        .unwrap_or_else(|| panic!("the search answered a {kind} array: {body}"))
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .filter(|id| id.starts_with("x_"))
        .map(str::to_owned)
        .collect()
}

/// Reads one member of the first catalog artist of a `search3` answer.
///
/// # Parameters
///
/// - `body`: the JSON response body.
/// - `member`: the member to read.
///
/// # Returns
///
/// The member's value, or `None` when the answer leaves it out.
fn catalog_artist_member(body: &str, member: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["subsonic-response"]["searchResult3"]["artist"][0]
        .get(member)
        .cloned()
}

/// A catalog artist card carries the provider's album count and a cover-art id
/// when the provider describes a picture, in both encodings and both searches.
#[tokio::test]
async fn a_search_card_carries_the_album_count_and_the_picture() {
    let fixture = Fixture::new().await;
    fixture
        .install_hello_catalog_with(serde_json::json!({
            "greeting": "Hello",
            "albumCount": 38,
            "coverUrl": "https://img.example/hello.jpg",
        }))
        .await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/search3?query=world&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    let artist = catalog_artist_member(&json.body, "id")
        .and_then(|id| id.as_str().map(str::to_owned))
        .expect("the catalog artist is listed");
    assert_eq!(
        catalog_artist_member(&json.body, "albumCount"),
        Some(serde_json::json!(38)),
        "the provider's count is the card's count: {}",
        json.body
    );
    assert_eq!(
        catalog_artist_member(&json.body, "coverArt"),
        Some(serde_json::json!(artist)),
        "a described picture is addressable through getCoverArt: {}",
        json.body
    );

    let xml = get(
        &app,
        &format!(
            "/rest/search3.view?query=world&apiKey={}&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("albumCount=\"38\""), "{}", xml.body);
    assert!(
        xml.body.contains(&format!("coverArt=\"{artist}\"")),
        "{}",
        xml.body
    );

    let two = get(
        &app,
        &format!(
            "/rest/search2?query=world&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&two);
    assert!(
        two.body.contains("\"albumCount\":38"),
        "the older search carries both members too: {}",
        two.body
    );
    assert!(
        two.body.contains(&format!("\"coverArt\":\"{artist}\"")),
        "{}",
        two.body
    );
}

/// A provider that states no album count and describes no picture leaves both
/// members out rather than answering zero and an unfetchable id.
#[tokio::test]
async fn a_search_card_without_extras_omits_them() {
    let fixture = Fixture::new().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/search3?query=world&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert_eq!(catalog_artist_member(&json.body, "albumCount"), None);
    assert_eq!(
        catalog_artist_member(&json.body, "coverArt"),
        Some(serde_json::json!("")),
        "an artist with no picture carries an empty cover-art id: {}",
        json.body
    );
}

/// The album count a page lists wins over the provider's own count.
#[tokio::test]
async fn a_catalog_artist_page_count_wins_over_the_payload_count() {
    let fixture = Fixture::new().await;
    fixture
        .install_hello_catalog_with(serde_json::json!({
            "greeting": "Hello",
            "albumCount": 38,
            "coverUrl": "https://img.example/hello.jpg",
        }))
        .await;
    let answer = get(
        &fixture.app(),
        &format!(
            "/rest/search3?query=world&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&answer);
    let artist = catalog_artist_member(&answer.body, "id")
        .and_then(|id| id.as_str().map(str::to_owned))
        .expect("the catalog artist is listed");
    fixture
        .enrich(&hearthsound_core::ClientId::parse(&artist).unwrap())
        .await;

    let page = get(
        &fixture.app(),
        &format!(
            "/rest/getArtist?id={artist}&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&page);
    let value: serde_json::Value = serde_json::from_str(&page.body).unwrap();
    assert_eq!(
        value["subsonic-response"]["artist"]["albumCount"],
        serde_json::json!(1),
        "the one album the page lists is the count: {}",
        page.body
    );
}

/// Returns the titles of the songs of a `search3` answer, in response order.
///
/// # Parameters
///
/// - `body`: the response body.
///
/// # Returns
///
/// The song titles, in the order the response lists them.
fn song_titles(body: &str) -> Vec<String> {
    let document: serde_json::Value = serde_json::from_str(body).unwrap();
    document["subsonic-response"]["searchResult3"]["song"]
        .as_array()
        .map(|songs| {
            songs
                .iter()
                .map(|song| song["title"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Saves the two albums the seeded account does not hold, stamps the instants
/// the orders read, and plays one song, so every song order has a sequence of
/// its own.
///
/// # Returns
///
/// The fixture and the seeded ids.
async fn ordered_songs() -> (Fixture, Seeded) {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    for album in [seeded.bright, seeded.delta] {
        let saved = get(
            &app,
            &format!(
                "/rest/hsSave?apiKey={}&hearthsound=1&f=json&id={album}",
                fixture.api_key
            ),
        )
        .await;
        assert_ok(&saved);
    }
    for (track, at) in [
        (seeded.alpha_tracks[0], 400),
        (seeded.alpha_tracks[1], 100),
        (seeded.bright_track, 200),
        (seeded.delta_track, 300),
    ] {
        sqlx::query("UPDATE tracks SET created_at = ? WHERE id = ?")
            .bind(at)
            .bind(track.uuid().as_bytes().as_slice())
            .execute(fixture.database.writer())
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO plays (id, user_id, track_id, started_at, listened_ms, audio_delivered) \
         VALUES (?, ?, ?, 500, 200000, 1)",
    )
    .bind(uuid::Uuid::now_v7().as_bytes().as_slice())
    .bind(fixture.user_id.uuid().as_bytes().as_slice())
    .bind(seeded.alpha_tracks[1].uuid().as_bytes().as_slice())
    .execute(fixture.database.writer())
    .await
    .unwrap();
    (fixture, seeded)
}

/// The vendor `sort` and `genre` parameters order and narrow the songs an
/// empty query lists, and a request without `hearthsound=1` is answered exactly
/// as before.
#[tokio::test]
async fn the_vendor_parameters_order_and_narrow_the_song_listing() {
    let (fixture, _) = ordered_songs().await;
    let app = fixture.app();

    let listing = |extra: &str| {
        format!(
            "/rest/search3?query=&musicFolderId=1&artistCount=0&albumCount=0&songCount=100\
             &apiKey={}&f=json{extra}",
            fixture.api_key
        )
    };
    let mut firsts = Vec::new();
    for (sort, expected) in [
        ("", vec!["Ocean", "One", "Signal", "Zero"]),
        ("&sort=added", vec!["Zero", "Ocean", "Signal", "One"]),
        ("&sort=played", vec!["One", "Ocean", "Signal", "Zero"]),
        ("&sort=artist", vec!["Signal", "Ocean", "One", "Zero"]),
        ("&sort=album", vec!["One", "Zero", "Signal", "Ocean"]),
        ("&sort=year", vec!["Ocean", "Signal", "One", "Zero"]),
    ] {
        let probe = get(&app, &listing(&format!("&hearthsound=1{sort}"))).await;
        assert_ok(&probe);
        assert_eq!(song_titles(&probe.body), expected, "sort={sort}");
        firsts.push(expected[0].to_owned());
    }
    assert_eq!(
        firsts,
        ["Ocean", "Zero", "One", "Signal", "One", "Ocean"],
        "each order starts where its own rule puts it"
    );

    // An unknown order falls back to the title order instead of failing.
    let unknown = get(&app, &listing("&hearthsound=1&sort=nonsense")).await;
    assert_ok(&unknown);
    assert_eq!(
        song_titles(&unknown.body),
        ["Ocean", "One", "Signal", "Zero"],
        "an unknown order is ignored"
    );

    // The genre filter narrows the listing, and composes with an order.
    let rock = get(&app, &listing("&hearthsound=1&genre=Rock")).await;
    assert_ok(&rock);
    assert_eq!(song_titles(&rock.body), ["One", "Zero"]);
    let ambient = get(&app, &listing("&hearthsound=1&genre=ambient&sort=year")).await;
    assert_ok(&ambient);
    assert_eq!(song_titles(&ambient.body), ["Ocean"]);
    let unknown_genre = get(&app, &listing("&hearthsound=1&genre=polka")).await;
    assert_ok(&unknown_genre);
    assert!(song_titles(&unknown_genre.body).is_empty());

    // Without the vendor opt-in the parameters change nothing, so a standard
    // client's answer is the one it always got.
    let plain = get(
        &app,
        &format!(
            "/rest/search3?query=&musicFolderId=1&artistCount=0&albumCount=0&songCount=100\
             &sort=year&genre=Rock&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&plain);
    assert_eq!(
        song_titles(&plain.body),
        ["Ocean", "One", "Signal", "Zero"],
        "a request without hearthsound=1 is answered in title order"
    );
}

/// Saving an album adds its songs to the account's Songs and its artist to its
/// Artists, and leaves another account's library alone.
#[tokio::test]
async fn saving_an_album_lists_its_songs_and_artist_for_that_account_only() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let (_, other_key) = fixture.account(&[]).await;

    let songs = |key: &str| {
        format!(
            "/rest/search3?query=&musicFolderId=1&artistCount=0&albumCount=0&songCount=100\
             &apiKey={key}&f=json"
        )
    };
    let artists = |key: &str| format!("/rest/getArtists?musicFolderId=1&apiKey={key}&f=json");

    // The album of a generic folder is in nobody's library, so its song and its
    // artist are in nobody's lists.
    let before = get(&app, &songs(&fixture.api_key)).await;
    assert_ok(&before);
    assert!(!before.body.contains(&seeded.bright_track.to_string()));
    let before_artists = get(&app, &artists(&fixture.api_key)).await;
    assert_ok(&before_artists);
    assert!(!before_artists.body.contains("2 Mello"));

    let saved = get(
        &app,
        &format!(
            "/rest/hsSave?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, seeded.bright
        ),
    )
    .await;
    assert_ok(&saved);

    let after = get(&app, &songs(&fixture.api_key)).await;
    assert_ok(&after);
    assert_eq!(
        song_titles(&after.body),
        ["One", "Signal", "Zero"],
        "the saved album's song joins the songs listing"
    );
    let after_artists = get(&app, &artists(&fixture.api_key)).await;
    assert_ok(&after_artists);
    assert!(
        after_artists.body.contains("2 Mello"),
        "the saved album's artist joins the artist index: {}",
        after_artists.body
    );

    // Another account's library holds none of it.
    let other_songs = get(&app, &songs(&other_key)).await;
    assert_ok(&other_songs);
    assert!(!other_songs.body.contains(&seeded.bright_track.to_string()));
    let other_artists = get(&app, &artists(&other_key)).await;
    assert_ok(&other_artists);
    assert!(!other_artists.body.contains("2 Mello"));
}
