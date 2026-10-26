//! Integration tests for the genre endpoints: the counts `getGenres` answers
//! over the seeded library, the songs a genre or a random sample returns, and
//! the parameters each endpoint requires.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};
use hearthsound_core::EntityId;

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

/// `getTopSongs` ranks an artist's songs by how often they were played.
#[tokio::test]
async fn top_songs_follow_the_play_counts() {
    let (fixture, seeded) = fixture().await;
    let activity = fixture.state.activity();
    let user = fixture.user_id.uuid();
    // The second song is played twice and the first once, and the other album's
    // song is played as often as the second one.
    for (track, times) in [
        (seeded.alpha_tracks[1], 2),
        (seeded.alpha_tracks[0], 1),
        (seeded.bright_track, 2),
    ] {
        for index in 0..times {
            activity
                .plays()
                .record_play(
                    user,
                    track.uuid(),
                    Some("Rufin"),
                    1_000 + index,
                    200_000,
                    true,
                )
                .await
                .unwrap();
        }
    }
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
    let first = listed.body.find("One").expect("the most played song");
    let second = listed.body.find("Zero").expect("the least played song");
    assert!(
        first < second,
        "the most played song comes first: {}",
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

/// `getSimilarSongs2` serves a stored suggestion as the stored song and a
/// catalog-only one as a catalog song, in both response shapes.
#[tokio::test]
async fn similar_songs_mix_stored_and_catalog_songs() {
    let (fixture, seeded) = fixture().await;
    let (stored, album, artist) = fixture
        .seed_track("Fixture Album", "Fixture Artist", "Fixture Song 1", 180_000)
        .await;
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
        json.body.contains(&stored.to_string()),
        "the stored suggestion is served as the stored song: {}",
        json.body
    );
    assert!(
        json.body.contains(catalog_track(2).as_str()),
        "the other suggestion is served as a catalog song: {}",
        json.body
    );
    let stabilized = stabilize(&json.body, stored, album, artist);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("similar-songs-mixed-json", stabilized);
    });

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
        xml.body.contains(&stored.to_string()),
        "the stored suggestion is served as the stored song: {}",
        xml.body
    );
    assert!(
        xml.body.contains(catalog_track(2).as_str()),
        "the other suggestion is served as a catalog song: {}",
        xml.body
    );
    let stabilized = stabilize(&xml.body, stored, album, artist);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("similar-songs-mixed-xml", stabilized);
    });
}

/// A song the discovery layer names twice is served once, at its first
/// position, in both suggestion listings.
#[tokio::test]
async fn similar_songs_list_each_song_once() {
    let (fixture, seeded) = fixture().await;
    let (stored, _album, _artist) = fixture
        .seed_track("Fixture Album", "Fixture Artist", "Fixture Song 1", 180_000)
        .await;
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
        song_ids(&similar.body, "similarSongs"),
        vec![catalog_track(2), stored.to_string()],
        "each suggestion is listed once, at its first position: {}",
        similar.body
    );

    let top = get(
        &app,
        &format!(
            "/rest/getTopSongs?id={}&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&top);
    assert_eq!(
        song_ids(&top.body, "topSongs"),
        vec![catalog_track(2)],
        "the duplicated top song is listed once: {}",
        top.body
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
    let ids = song_ids(&listed.body, "similarSongs2");
    assert_eq!(
        ids,
        vec![catalog_track(1), catalog_track(2)],
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
        song_ids(&capped.body, "similarSongs2"),
        vec![catalog_track(1), catalog_track(2)],
        "the album seeds the same artists and the count caps the listing: {}",
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
        listed.body.contains(catalog_track(1).as_str()),
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
    assert!(first.body.contains(catalog_track(1).as_str()));

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
        again.body.contains(catalog_track(1).as_str()),
        "the cached answer is served: {}",
        again.body
    );
    assert!(
        !again.body.contains(catalog_track(2).as_str()),
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
        other.body.contains(catalog_track(2).as_str()),
        "a new argument asks the instance again: {}",
        other.body
    );
}

/// `getTopSongs` prefers the discovery layer and falls back to the library's
/// like counts when no instance answers.
#[tokio::test]
async fn top_songs_use_discovery_then_likes() {
    let (first, seeded) = fixture().await;
    first
        .install_album_catalog(serde_json::json!({
            "searchResults": [{ "text": "Fixture Song 2", "track": 2 }],
        }))
        .await;
    first
        .install_discovery_layer(&[(
            "scrobble",
            serde_json::json!({
                "topTracks": [
                    { "title": "Fixture Song 2", "artists": ["Fixture Artist"] },
                ],
            }),
        )])
        .await;
    let app = first.app();

    let discovered = get(
        &app,
        &format!(
            "/rest/getTopSongs?id={}&apiKey={}&f=json",
            seeded.cardinals, first.api_key
        ),
    )
    .await;
    assert_ok(&discovered);
    assert!(
        discovered.body.contains(catalog_track(2).as_str()),
        "the discovery layer answered: {}",
        discovered.body
    );
    assert!(
        !discovered.body.contains("Zero"),
        "the library fallback did not answer: {}",
        discovered.body
    );

    // A second fixture has a discovery layer with nothing configured, so the
    // stored artist's songs are ranked by the library.
    let (ranked, ranked_seeded) = fixture().await;
    ranked
        .state
        .activity()
        .set_feedback(
            ranked.user_id.uuid(),
            hearthsound_core::EntityKind::Track,
            ranked_seeded.alpha_tracks[0].uuid(),
            hearthsound_activity::Feedback::Like,
        )
        .await
        .unwrap();
    let ranked_app = ranked.app();
    let listed = get(
        &ranked_app,
        &format!(
            "/rest/getTopSongs?id={}&apiKey={}&f=json",
            ranked_seeded.cardinals, ranked.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    let first = listed.body.find("Zero").expect("the liked song");
    let second = listed.body.find("One").expect("the unliked song");
    assert!(
        first < second,
        "the liked song comes first when neither was played: {}",
        listed.body
    );
}

/// Returns the client id of one track of the album catalog fixture.
///
/// # Parameters
///
/// - `number`: the track number within the fixture's album.
fn catalog_track(number: u8) -> String {
    hearthsound_core::CatalogId::new(
        hearthsound_core::Slug::new("album").expect("the fixture slug is valid"),
        hearthsound_core::EntityKind::Track,
        format!("album-catalog-track-{number}"),
    )
    .expect("the fixture id is valid")
    .to_string()
}

/// Replaces the ids of a freshly seeded track with names, so a snapshot of a
/// response that carries them stays stable.
fn stabilize(body: &str, track: EntityId, album: EntityId, artist: EntityId) -> String {
    body.replace(&track.to_string(), "STORED_TRACK")
        .replace(&album.to_string(), "STORED_ALBUM")
        .replace(&artist.to_string(), "STORED_ARTIST")
        .replace(&catalog_track(2), "CATALOG_TRACK")
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

/// A stored artist's top songs include the catalog chart its counterpart reports
/// once the enrichment run has cached it.
///
/// The chart is what makes the panel the artist's most popular songs rather than
/// the most played files; a library-only read shows one song and never the other
/// five. The local rows stay first and a chart song the library already lists is
/// not repeated.
#[tokio::test]
async fn a_stored_artists_top_songs_include_the_cached_catalog_chart() {
    use hearthsound_core::ClientId;

    let (fixture, seeded) = fixture().await;
    fixture.install_discography_catalog().await;
    sqlx::query(
        "INSERT INTO external_ids (namespace, value, entity_kind, entity_id) \
         VALUES ('p:disco', 'disco-artist-1', 'artist', ?)",
    )
    .bind(seeded.cardinals.uuid().as_bytes().as_slice())
    .execute(fixture.database.writer())
    .await
    .unwrap();
    fixture.enrich(&ClientId::Entity(seeded.cardinals)).await;
    let app = fixture.app();

    let listed = get(
        &app,
        &format!(
            "/rest/getTopSongs?id={}&hearthsound=1&count=10&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    let ids = song_ids(&listed.body, "topSongs");
    assert!(
        ids.iter().any(|id| id.starts_with("x_disco_t_")),
        "the cached chart is listed: {}",
        listed.body
    );
    assert!(
        listed.body.contains("First Light"),
        "the chart's first track is named: {}",
        listed.body
    );
    let mut titles: Vec<String> = serde_json::from_str::<serde_json::Value>(&listed.body).unwrap()
        ["subsonic-response"]["topSongs"]["song"]
        .as_array()
        .unwrap()
        .iter()
        .map(|song| song["title"].as_str().unwrap().to_owned())
        .collect();
    let count = titles.len();
    titles.sort();
    titles.dedup();
    assert_eq!(
        count,
        titles.len(),
        "no song is listed twice: {}",
        listed.body
    );
}
