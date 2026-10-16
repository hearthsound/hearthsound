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
