//! Integration tests for `search3`: the local-first ordering, the provider
//! results the search layer resolves into rows, the scope rule, and the
//! empty-query listing.
//!
//! The catalog side is the `hello-catalog` fixture installed through the
//! provider registry, so the rows a snapshot shows are exactly what a real
//! provider would contribute.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_config::{SettingValue, keys};

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

/// The filters that make a search answer stable across runs.
///
/// A provider hit is resolved into a row, so its client id carries the UUIDv7
/// the insert generated; the filters replace every entity id with a marker and
/// the filters of [`SNAPSHOT_FILTERS`] replace the row's creation instant.
///
/// # Returns
///
/// The filters.
fn stable_filters() -> Vec<(&'static str, &'static str)> {
    let mut filters: Vec<(&'static str, &'static str)> = SNAPSHOT_FILTERS.to_vec();
    filters.push((r"(?:ar|al|tr)_[0-9a-z]{20,}", "ID"));
    filters
}

#[tokio::test]
async fn search3_answers_local_results_then_provider_rows() {
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
    assert!(json.body.contains("Hello"), "{}", json.body);
    insta::with_settings!({filters => stable_filters()}, {
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
    insta::with_settings!({filters => stable_filters()}, {
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
    insta::with_settings!({filters => stable_filters()}, {
        insta::assert_snapshot!("empty-query-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!("/rest/search3.view?query=&apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => stable_filters()}, {
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
        !probe.body.contains("Hello"),
        "a disabled catalog contributes nothing: {}",
        probe.body
    );
}

#[tokio::test]
async fn search3_drops_a_catalog_track_the_server_already_stores() {
    let (fixture, _seeded) = fixture().await;
    fixture.install_hello_catalog().await;

    // A local track the hello catalog also returns: the same title, artist, and
    // duration, so the search answers the stored row once instead of listing the
    // provider's copy beside it.
    let local = fixture
        .seed_track("Hello Album", "Hello Artist", "Hello, cardinals", 180_000)
        .await
        .0;
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
    assert_eq!(
        probe.body.matches(&local.to_string()).count(),
        1,
        "the stored recording is listed once: {}",
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
    // The artist is a bare reference, the album is a folder, and a provider hit
    // is a row like any other.
    assert!(
        json.body.contains(&format!(
            "{{\"id\":\"{}\",\"name\":\"The Cardinals\"}}",
            seeded.cardinals
        )),
        "the artist carries its id and name: {}",
        json.body
    );
    assert!(json.body.contains("\"isDir\":true"), "{}", json.body);
    insta::with_settings!({filters => stable_filters()}, {
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
    insta::with_settings!({filters => stable_filters()}, {
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
}

/// A search reads the caller's personal library, so content the account has not
/// adopted is not a hit; the tree itself is still browsable by an administrator.
#[tokio::test]
async fn a_search_covers_the_personal_library() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let mello = seeded.mello.to_string();
    let cardinals = seeded.cardinals.to_string();

    let searched = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&searched);
    assert!(
        searched.body.contains(&cardinals),
        "the account's own artist is a hit: {}",
        searched.body
    );

    // The generic folder's artist is not in the account's personal library, so
    // the search does not answer with it.
    let other = get(
        &app,
        &format!(
            "/rest/search3?query=mello&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&other);
    assert!(
        !other.body.contains(&mello),
        "a search reads the personal library: {}",
        other.body
    );

    // The tree still holds it, which is what the browse endpoints read.
    let browsed = get(
        &app,
        &format!(
            "/rest/getArtist?id={mello}&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&browsed);
    assert!(
        browsed.body.contains("2 Mello"),
        "the server view still browses it: {}",
        browsed.body
    );
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
    insta::with_settings!({filters => stable_filters()}, {
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
    insta::with_settings!({filters => stable_filters()}, {
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
    insta::with_settings!({filters => stable_filters()}, {
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
