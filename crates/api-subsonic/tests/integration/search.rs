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
