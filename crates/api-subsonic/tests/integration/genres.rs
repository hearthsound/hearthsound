//! Integration tests for the genre endpoints: the counts `getGenres` answers
//! over the seeded library, the songs a genre or a random sample returns, and
//! the parameters each endpoint requires.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Counts the songs of a listing response.
///
/// # Parameters
///
/// - `body`: the JSON body of the response.
/// - `member`: the member the listing is carried in.
///
/// # Returns
///
/// The number of songs; a listing with no songs counts zero.
fn song_count(body: &str, member: &str) -> usize {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["subsonic-response"][member]["song"]
        .as_array()
        .map_or(0, |songs| songs.len())
}

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

/// `getGenres` counts the genres of the tracks in scope.
#[tokio::test]
async fn get_genres_counts_the_scope() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // The personal scope counts only what the account owns: two Rock songs of
    // one album and one Pop song, and none of the server's other genres.
    let json = get(
        &app,
        &format!(
            "/rest/getGenres?musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"genres\""), "{}", json.body);
    assert!(
        json.body
            .contains("{\"songCount\":2,\"albumCount\":1,\"value\":\"rock\"}"),
        "{}",
        json.body
    );
    assert!(
        json.body
            .contains("{\"songCount\":1,\"albumCount\":1,\"value\":\"pop\"}"),
        "{}",
        json.body
    );
    assert!(
        !json.body.contains("electronic"),
        "a genre only another folder carries is not listed: {}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("genres-json", seeded.stabilize(&json.body));
    });

    // The server view holds everything stored, so it counts every genre.
    let server = get(
        &app,
        &format!(
            "/rest/getGenres.view?musicFolderId=2&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&server);
    assert!(server.body.contains("electronic"), "{}", server.body);
    assert!(server.body.contains("ambient"), "{}", server.body);
    assert!(server.body.contains("rock"), "{}", server.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("genres-xml", seeded.stabilize(&server.body));
    });

    let unknown = get(
        &app,
        &format!(
            "/rest/getGenres?musicFolderId=9&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);
}

/// `getSongsByGenre` lists the songs of one genre in scope.
#[tokio::test]
async fn get_songs_by_genre_lists_one_genre() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let listed = |query: &str| {
        let app = app.clone();
        let api_key = fixture.api_key.clone();
        let query = query.to_owned();
        async move {
            get(
                &app,
                &format!("/rest/getSongsByGenre?{query}&musicFolderId=1&apiKey={api_key}&f=json"),
            )
            .await
        }
    };

    // The genre is matched without regard to case, and the songs come back in
    // title order.
    let rock = listed("genre=rock").await;
    assert_ok(&rock);
    assert!(
        rock.body.contains(&seeded.alpha_tracks[0].to_string()),
        "{}",
        rock.body
    );
    assert!(
        rock.body.contains(&seeded.alpha_tracks[1].to_string()),
        "{}",
        rock.body
    );
    assert!(
        rock.body.find("One").unwrap() < rock.body.find("Zero").unwrap(),
        "the songs are ordered by title: {}",
        rock.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("songs-by-genre-json", seeded.stabilize(&rock.body));
    });

    let one = listed("genre=Rock&count=1").await;
    assert_ok(&one);
    assert_eq!(song_count(&one.body, "songsByGenre"), 1, "{}", one.body);
    let second = listed("genre=Rock&count=1&offset=1").await;
    assert_ok(&second);
    assert!(
        second.body.contains(&seeded.alpha_tracks[0].to_string()),
        "the offset skips the first song: {}",
        second.body
    );

    // A genre no track in scope carries is an empty listing, not an error.
    let empty = listed("genre=Electronic").await;
    assert_ok(&empty);
    assert!(
        empty.body.contains("\"song\":[]"),
        "an empty genre answers an empty listing: {}",
        empty.body
    );

    let missing = get(
        &app,
        &format!(
            "/rest/getSongsByGenre?musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&missing, 10);
    let bad_count = listed("genre=Rock&count=many").await;
    assert_error(&bad_count, 0);
}

/// `getRandomSongs` samples the songs in scope and honours its filters.
#[tokio::test]
async fn get_random_songs_samples_the_scope() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let sampled = |query: &str| {
        let app = app.clone();
        let api_key = fixture.api_key.clone();
        let query = query.to_owned();
        async move {
            get(
                &app,
                &format!("/rest/getRandomSongs?{query}&musicFolderId=1&apiKey={api_key}&f=json"),
            )
            .await
        }
    };

    // The owned folder holds two songs, so a larger sample still returns two.
    let all = sampled("size=100").await;
    assert_ok(&all);
    assert_eq!(song_count(&all.body, "randomSongs"), 2, "{}", all.body);
    assert!(
        all.body.contains(&seeded.alpha_tracks[0].to_string()),
        "{}",
        all.body
    );
    assert!(
        all.body.contains(&seeded.alpha_tracks[1].to_string()),
        "{}",
        all.body
    );

    // The default size is ten, which the two songs of the scope fill.
    let defaulted = sampled("").await;
    assert_ok(&defaulted);
    assert_eq!(
        song_count(&defaulted.body, "randomSongs"),
        2,
        "{}",
        defaulted.body
    );

    let pop = sampled("genre=Pop").await;
    assert_ok(&pop);
    assert_eq!(song_count(&pop.body, "randomSongs"), 1, "{}", pop.body);
    assert!(
        pop.body.contains(&seeded.alpha_tracks[1].to_string()),
        "{}",
        pop.body
    );

    // The album is from 1999, so a bound of 2000 excludes both songs.
    let recent = sampled("fromYear=2000").await;
    assert_ok(&recent);
    assert!(
        recent.body.contains("\"song\":[]"),
        "a year bound excludes the album: {}",
        recent.body
    );
    let older = sampled("toYear=1999").await;
    assert_ok(&older);
    assert_eq!(song_count(&older.body, "randomSongs"), 2, "{}", older.body);
    let ranged = sampled("fromYear=1999&toYear=1999").await;
    assert_ok(&ranged);
    assert_eq!(
        song_count(&ranged.body, "randomSongs"),
        2,
        "{}",
        ranged.body
    );

    let bad_size = sampled("size=lots").await;
    assert_error(&bad_size, 0);
    let bad_year = sampled("fromYear=long-ago").await;
    assert_error(&bad_year, 0);
    let unknown = get(
        &app,
        &format!(
            "/rest/getRandomSongs?musicFolderId=9&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);
}

/// `getTopSongs` lists the artist's own songs, and a name no artist has answers
/// an empty listing.
#[tokio::test]
async fn top_songs_list_the_artists_tracks() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let listed = get(
        &app,
        &format!(
            "/rest/getTopSongs?id={}&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert!(listed.body.contains("\"topSongs\""), "{}", listed.body);
    assert!(
        listed.body.contains(&seeded.alpha_tracks[0].to_string()),
        "the artist's songs are listed: {}",
        listed.body
    );
    assert!(
        !listed.body.contains(&seeded.bright_track.to_string()),
        "another artist's song is not listed: {}",
        listed.body
    );

    // A name that no artist has answers an empty listing rather than an error.
    let unknown = get(
        &app,
        &format!(
            "/rest/getTopSongs?artist=Nobody&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&unknown);
    assert!(unknown.body.contains("\"song\":[]"), "{}", unknown.body);

    let missing = get(
        &app,
        &format!("/rest/getTopSongs?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing, 10);
}

/// `getSimilarSongs` answers an empty listing and requires an id.
#[tokio::test]
async fn similar_songs_answer_an_empty_listing() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let listed = get(
        &app,
        &format!(
            "/rest/getSimilarSongs?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"similarSongs\":{\"song\":[]}"),
        "{}",
        listed.body
    );

    let listed = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2.view?id={}&apiKey={}",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert!(listed.body.contains("similarSongs2"), "{}", listed.body);

    let missing = get(
        &app,
        &format!("/rest/getSimilarSongs?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing, 10);
}

/// `getSimilarSongs2` serves every resolved suggestion as the row it names, in
/// both response shapes.
///
/// The discovery fixture suggests two tracks; each is resolved to a row and
/// projected as a stored song, so the listing carries no provider id at all.
#[tokio::test]
async fn similar_songs_serve_each_resolved_suggestion() {
    let (fixture, seeded) = fixture().await;
    fixture
        .install_album_catalog(serde_json::json!({
            "searchResults": [{ "text": "Fixture Song 2", "track": 2 }],
        }))
        .await;
    fixture
        .install_discovery_layer(&[(
            "scrobble",
            serde_json::json!({
                "similarTracks": [
                    { "title": "Fixture Song 1", "artists": ["Fixture Artist"] },
                    { "title": "Fixture Song 2", "artists": ["Fixture Artist"] },
                ],
            }),
        )])
        .await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(
        json.body.contains("Fixture Song 1") && json.body.contains("Fixture Song 2"),
        "both suggestions are listed: {}",
        json.body
    );
    for id in song_ids(&json.body, "similarSongs2") {
        assert!(
            id.starts_with("tr_"),
            "a suggestion is served by its row's id, not {id}: {}",
            json.body
        );
    }

    let xml = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2.view?id={}&apiKey={}",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(
        xml.body.contains("Fixture Song 2"),
        "the older shape lists the same suggestions: {}",
        xml.body
    );
}

/// Returns the titles of one song listing, in order.
///
/// # Parameters
///
/// - `body`: the rendered response.
/// - `member`: the member the listing is carried in.
fn song_titles(body: &str, member: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["subsonic-response"][member]["song"]
        .as_array()
        .unwrap()
        .iter()
        .map(|song| song["title"].as_str().unwrap().to_owned())
        .collect()
}

/// Every suggestion the discovery layer names is listed, in its order.
#[tokio::test]
async fn similar_songs_list_every_suggestion() {
    let (fixture, seeded) = fixture().await;
    fixture
        .install_album_catalog(serde_json::json!({
            "searchResults": [{ "text": "Fixture Song 2", "track": 2 }],
        }))
        .await;
    fixture
        .install_discovery_layer(&[(
            "scrobble",
            serde_json::json!({
                "similarTracks": [
                    { "title": "Fixture Song 2", "artists": ["Fixture Artist"] },
                    { "title": "Fixture Song 1", "artists": ["Fixture Artist"] },
                    { "title": "Fixture Song 2", "artists": ["Fixture Artist"] },
                    { "title": "Fixture Song 1", "artists": ["Fixture Artist"] },
                ],
                "topTracks": [
                    { "title": "Fixture Song 2", "artists": ["Fixture Artist"] },
                    { "title": "Fixture Song 2", "artists": ["Fixture Artist"] },
                ],
            }),
        )])
        .await;
    let app = fixture.app();

    let similar = get(
        &app,
        &format!(
            "/rest/getSimilarSongs?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&similar);
    assert_eq!(
        song_titles(&similar.body, "similarSongs"),
        vec![
            "Fixture Song 2".to_owned(),
            "Fixture Song 1".to_owned(),
            "Fixture Song 2".to_owned(),
            "Fixture Song 1".to_owned(),
        ],
        "the suggestions keep their order: {}",
        similar.body
    );
}

/// `getSimilarSongs2` seeds similar artists from an album or artist id and
/// interleaves their popular tracks, listing a song both artists name once.
#[tokio::test]
async fn similar_songs_from_an_artist_interleave_artist_tracks() {
    let (fixture, seeded) = fixture().await;
    fixture
        .install_album_catalog(serde_json::json!({
            "searchResults": [
                { "text": "Fixture Song 1", "track": 1 },
                { "text": "Fixture Song 2", "track": 2 },
            ],
        }))
        .await;
    fixture
        .install_discovery_layer(&[(
            "scrobble",
            serde_json::json!({
                "similarArtists": [{ "name": "First" }, { "name": "Second" }],
                "topTracks": [
                    { "title": "Fixture Song 1", "artists": ["Fixture Artist"] },
                    { "title": "Fixture Song 2", "artists": ["Fixture Artist"] },
                ],
            }),
        )])
        .await;
    let app = fixture.app();

    let listed = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2?id={}&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert_eq!(
        song_titles(&listed.body, "similarSongs2"),
        vec!["Fixture Song 1".to_owned(), "Fixture Song 2".to_owned()],
        "the artists' tracks are interleaved once each: {}",
        listed.body
    );

    let capped = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2?id={}&apiKey={}&count=3&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&capped);
    assert_eq!(
        song_ids(&capped.body, "similarSongs2").len(),
        2,
        "the album seeds the same artists: {}",
        capped.body
    );
}

/// The discovery layer skips an instance that cannot answer and asks the next.
#[tokio::test]
async fn a_failing_discovery_instance_is_skipped() {
    let (fixture, seeded) = fixture().await;
    fixture
        .install_album_catalog(serde_json::json!({
            "searchResults": [{ "text": "Fixture Song 1", "track": 1 }],
        }))
        .await;
    fixture
        .install_discovery_layer(&[
            ("broken", serde_json::json!({ "failDiscovery": true })),
            (
                "working",
                serde_json::json!({
                    "similarTracks": [
                        { "title": "Fixture Song 1", "artists": ["Fixture Artist"] },
                    ],
                }),
            ),
        ])
        .await;
    let app = fixture.app();

    let listed = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("Fixture Song 1"),
        "the working instance answered: {}",
        listed.body
    );
}

/// A cached discovery answer is served without asking the instance again.
#[tokio::test]
async fn a_cached_discovery_answer_is_not_asked_again() {
    let (fixture, seeded) = fixture().await;
    fixture
        .install_album_catalog(serde_json::json!({
            "searchResults": [
                { "text": "Fixture Song 1", "track": 1 },
                { "text": "Fixture Song 2", "track": 2 },
            ],
        }))
        .await;
    fixture
        .install_discovery_layer(&[(
            "scrobble",
            serde_json::json!({
                "similarTracks": [
                    { "title": "Fixture Song 1", "artists": ["Fixture Artist"] },
                ],
            }),
        )])
        .await;
    let app = fixture.app();
    let first = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&first);
    assert!(first.body.contains("Fixture Song 1"));

    fixture
        .update_discovery_config(
            "scrobble",
            serde_json::json!({
                "similarTracks": [
                    { "title": "Fixture Song 2", "artists": ["Fixture Artist"] },
                ],
            }),
        )
        .await;

    let again = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&again);
    assert!(
        again.body.contains("Fixture Song 1"),
        "the cached answer is served: {}",
        again.body
    );
    assert!(
        !again.body.contains("Fixture Song 2"),
        "the instance was not asked again: {}",
        again.body
    );

    // Another limit is another question, so it asks the instance and sees the
    // configuration it now has.
    let other = get(
        &app,
        &format!(
            "/rest/getSimilarSongs2?id={}&apiKey={}&count=5&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&other);
    assert!(
        other.body.contains("Fixture Song 2"),
        "a new argument asks the instance again: {}",
        other.body
    );
}

/// `getTopSongs` lists the stored chart first, then the artist's own library
/// tracks, and falls back to the library alone when no chart is stored.
#[tokio::test]
async fn top_songs_read_the_stored_chart_then_the_library() {
    let (chart, seeded) = fixture().await;
    let mut conn = chart.database.writer().acquire().await.unwrap();
    for (position, track) in [seeded.alpha_tracks[1], seeded.alpha_tracks[0]]
        .into_iter()
        .enumerate()
    {
        sqlx::query(
            "INSERT INTO artist_top_tracks (artist_id, position, track_id) VALUES (?, ?, ?)",
        )
        .bind(seeded.cardinals.uuid().as_bytes().as_slice())
        .bind(i64::try_from(position).unwrap())
        .bind(track.uuid().as_bytes().as_slice())
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    drop(conn);
    let app = chart.app();

    let listed = get(
        &app,
        &format!(
            "/rest/getTopSongs?id={}&apiKey={}&f=json",
            seeded.cardinals, chart.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert_eq!(
        song_titles(&listed.body, "topSongs"),
        vec!["One".to_owned(), "Zero".to_owned()],
        "the stored chart answers first: {}",
        listed.body
    );

    // Another artist has no chart, so its own library tracks answer.
    let (bare, bare_seeded) = fixture().await;
    let bare_app = bare.app();
    let library_only = get(
        &bare_app,
        &format!(
            "/rest/getTopSongs?id={}&hearthsound=1&apiKey={}&f=json",
            bare_seeded.mello, bare.api_key
        ),
    )
    .await;
    assert_ok(&library_only);
    assert_eq!(
        song_titles(&library_only.body, "topSongs"),
        vec!["Signal".to_owned()],
        "the artist's library tracks answer without a chart: {}",
        library_only.body
    );
}

/// Reads the ids of a response's song listing.
fn song_ids(body: &str, member: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["subsonic-response"][member]["song"]
        .as_array()
        .unwrap()
        .iter()
        .map(|song| song["id"].as_str().unwrap().to_owned())
        .collect()
}
