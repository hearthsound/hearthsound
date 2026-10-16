//! Full-stack tests of the client checklist.
//!
//! Every test builds the real server over a temporary data directory, writes
//! generated audio into a library folder, scans it through `startScan`, and then
//! drives the `/rest` surface the way a client does: token and API-key
//! authentication, browsing, playback with a range and a transcode, the personal
//! data a user changes, the reports a player sends, and the administration
//! endpoints. What the tests assert is what a client reads back, so a break in
//! any layer between the router and the file shows up here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use hearthsound_core::EntityId;
use hearthsound_library::LibraryRepository;
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_TYPE, RANGE};
use serde_json::Value;

use crate::common::{Fixture, USERNAME};

/// The artist the seeded files are tagged with.
const ARTIST: &str = "Checklist Artist";

/// The album the seeded files are tagged with.
const ALBUM: &str = "Checklist Album";

/// The titles of the seeded files, in track order.
const TITLES: [&str; 2] = ["Checklist One", "Checklist Two"];

/// How long each seeded file lasts, in seconds.
const SECONDS: f64 = 3.0;

/// How long a scan or a report is given to take effect.
const PATIENCE: Duration = Duration::from_secs(30);

/// A library the checklist drives.
struct Library {
    /// The folder the files were written into.
    folder: PathBuf,
    /// The stored tracks, in track order.
    tracks: Vec<EntityId>,
    /// The stored album.
    album: EntityId,
    /// The stored artist.
    artist: EntityId,
}

/// Writes the audio into a library folder and scans it.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
///
/// # Returns
///
/// The stored library.
async fn seed_library(fixture: &Fixture) -> Library {
    let root = fixture.services.config().paths.library.clone();
    let folder = root.join("checklist");
    let album_dir = folder.join(ARTIST).join(ALBUM);
    std::fs::create_dir_all(&album_dir).unwrap();
    for (index, title) in TITLES.iter().enumerate() {
        let file_name = format!("{:02} - {title}.mp3", index + 1);
        AudioFixture::new(AudioFormat::Mp3Cbr)
            .seconds(SECONDS)
            .file_name(file_name)
            .tags(
                TrackTags::new(*title, ARTIST)
                    .album(ALBUM)
                    .album_artist(ARTIST),
            )
            .cover_png(64, 0x0033_66ff)
            .write_into(&album_dir)
            .await
            .unwrap();
    }

    LibraryRepository::new(fixture.services.database())
        .add_library_folder(&folder, Some(fixture.user_id))
        .await
        .unwrap();

    // A client starts the scan and polls the status until it is done, which is
    // exactly what the checklist does.
    let started = fixture
        .get(&format!(
            "/rest/startScan?apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    assert_eq!(started.status(), StatusCode::OK);
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let status = get_json(
            fixture,
            &format!("/rest/getScanStatus?apiKey={}&f=json", fixture.api_key),
        )
        .await;
        if status["scanStatus"]["scanning"] == Value::Bool(false) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the scan did not finish in time: {status}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let artists = get_json(
        fixture,
        &format!(
            "/rest/getArtists?musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    let artist = parse_id(&id_of(&artists, ARTIST));
    let albums = get_json(
        fixture,
        &format!(
            "/rest/getArtist?id={artist}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    let album = parse_id(&id_of(&albums, ALBUM));
    let detail = get_json(
        fixture,
        &format!(
            "/rest/getAlbum?id={album}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    let tracks: Vec<EntityId> = detail["album"]["song"]
        .as_array()
        .unwrap()
        .iter()
        .map(|song| parse_id(song["id"].as_str().unwrap()))
        .collect();
    assert_eq!(tracks.len(), TITLES.len(), "the scan stored both files");

    Library {
        folder,
        tracks,
        album,
        artist,
    }
}

/// Sends a GET request and returns the envelope's members.
///
/// The `subsonic-response` wrapper is unwrapped, so a caller reads a member as
/// `body["scanStatus"]`, which is what the protocol documents.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `path`: the request target.
///
/// # Returns
///
/// The parsed envelope, without its wrapper.
async fn get_json(fixture: &Fixture, path: &str) -> Value {
    let response = fixture.get(path).await;
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    let text = response.text().await.unwrap();
    let envelope: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{path} answered {text}: {error}"));
    envelope["subsonic-response"].clone()
}

/// Returns the id of the first entity a listing carries under a name.
///
/// # Parameters
///
/// - `value`: the response body.
/// - `name`: the name to find.
///
/// # Returns
///
/// The entity's client id.
fn id_of(value: &Value, name: &str) -> String {
    let found = find_named(value, name);
    found.unwrap_or_else(|| panic!("no entity named `{name}` in {value}"))
}

/// Finds an entity's id by name anywhere in a response.
///
/// # Parameters
///
/// - `value`: the response body.
/// - `name`: the name to find.
///
/// # Returns
///
/// The entity's client id, or `None` when the body carries no such name.
fn find_named(value: &Value, name: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            let named = map.get("name").and_then(Value::as_str) == Some(name)
                || map.get("title").and_then(Value::as_str) == Some(name);
            if named && let Some(id) = map.get("id").and_then(Value::as_str) {
                return Some(id.to_owned());
            }
            map.values().find_map(|child| find_named(child, name))
        }
        Value::Array(items) => items.iter().find_map(|child| find_named(child, name)),
        _ => None,
    }
}

/// Parses a client id into the entity id it names.
///
/// # Parameters
///
/// - `value`: the client id.
///
/// # Returns
///
/// The entity id.
fn parse_id(value: &str) -> EntityId {
    match hearthsound_core::ClientId::parse(value).unwrap() {
        hearthsound_core::ClientId::Entity(id) => id,
        hearthsound_core::ClientId::Catalog(id) => panic!("expected a stored id, got {id}"),
    }
}

/// The token a client computes for the salted-token login.
///
/// # Parameters
///
/// - `secret`: the account's secret.
/// - `salt`: the salt the client chose.
///
/// # Returns
///
/// The lowercase hexadecimal MD5 of the secret followed by the salt.
fn token(secret: &str, salt: &str) -> String {
    use md5::{Digest, Md5};
    Md5::digest(format!("{secret}{salt}"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Both logins work, and the browse endpoints answer the scanned library.
#[tokio::test]
async fn login_and_browse_answer_the_library() {
    let fixture = Fixture::new().await;
    let library = seed_library(&fixture).await;

    // The token login is what a client that stores an app password uses.
    let salt = "checklistsalt";
    let hashed = token(&fixture.app_password, salt);
    let token_login = get_json(
        &fixture,
        &format!("/rest/ping?u={USERNAME}&t={hashed}&s={salt}&v=1.16.1&c=checklist&f=json"),
    )
    .await;
    assert_eq!(token_login["status"], "ok");

    // The API-key login is what a client that stores a key uses, and it is the
    // one every other request in this file uses.
    let key_login = get_json(
        &fixture,
        &format!("/rest/ping?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(key_login["status"], "ok");

    let artists = get_json(
        &fixture,
        &format!(
            "/rest/getArtists?musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(
        find_named(&artists, ARTIST).as_deref(),
        Some(library.artist.to_string().as_str()),
        "the scanned artist is listed: {artists}"
    );

    let album = get_json(
        &fixture,
        &format!(
            "/rest/getAlbum?id={}&apiKey={}&f=json",
            library.album, fixture.api_key
        ),
    )
    .await;
    assert_eq!(album["album"]["name"], ALBUM);
    assert_eq!(album["album"]["songCount"], TITLES.len());

    let genres = get_json(
        &fixture,
        &format!(
            "/rest/getGenres?musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert!(
        genres["genres"]["genre"].is_array(),
        "the genre list answers: {genres}"
    );

    let indexes = get_json(
        &fixture,
        &format!(
            "/rest/getIndexes?musicFolderId=1&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert!(indexes["indexes"]["lastModified"].is_number(), "{indexes}");

    let directory = get_json(
        &fixture,
        &format!(
            "/rest/getMusicDirectory?id={}&apiKey={}&f=json",
            library.artist, fixture.api_key
        ),
    )
    .await;
    assert_eq!(directory["directory"]["name"], ARTIST);
    assert_eq!(
        directory["directory"]["child"][0]["id"],
        library.album.to_string()
    );
}

/// A stream is served whole, from a range, and as a transcode.
#[tokio::test]
async fn playback_serves_the_file_a_range_and_a_transcode() {
    let fixture = Fixture::new().await;
    let library = seed_library(&fixture).await;
    let track = library.tracks[0];

    let whole = fixture
        .get(&format!(
            "/rest/stream?id={track}&apiKey={}",
            fixture.api_key
        ))
        .await;
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(whole.headers()[CONTENT_TYPE], "audio/mpeg");
    let bytes = whole.bytes().await.unwrap();
    assert!(!bytes.is_empty(), "the file's bytes are served");

    // A range is what a client that seeks sends.
    let part = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={track}&apiKey={}",
            fixture.base_url(),
            fixture.api_key
        ))
        .header(RANGE, "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    let part = part.bytes().await.unwrap();
    assert_eq!(part.len(), 100);
    assert_eq!(part.as_ref(), &bytes[..100]);

    // A cap and a format are what a client that cannot play the file sends.
    let transcoded = fixture
        .get(&format!(
            "/rest/stream?id={track}&maxBitRate=128&format=mp3&apiKey={}",
            fixture.api_key
        ))
        .await;
    assert_eq!(transcoded.status(), StatusCode::OK);
    let transcoded = transcoded.bytes().await.unwrap();
    assert!(!transcoded.is_empty(), "the encoded stream carries bytes");
    assert_ne!(
        transcoded.as_ref(),
        bytes.as_ref(),
        "a capped stream is not the stored file"
    );
}

/// The personal data a user changes is what the next request reads back.
#[tokio::test]
async fn personal_data_round_trips() {
    let fixture = Fixture::new().await;
    let library = seed_library(&fixture).await;
    let track = library.tracks[0];

    // Stars.
    let starred = get_json(
        &fixture,
        &format!(
            "/rest/star?id={track}&albumId={}&artistId={}&apiKey={}&f=json",
            library.album, library.artist, fixture.api_key
        ),
    )
    .await;
    assert_eq!(starred["status"], "ok");
    let listed = get_json(
        &fixture,
        &format!("/rest/getStarred2?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(
        listed["starred2"]["song"][0]["id"],
        track.to_string(),
        "{listed}"
    );
    let unstarred = get_json(
        &fixture,
        &format!("/rest/unstar?id={track}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(unstarred["status"], "ok");
    let listed = get_json(
        &fixture,
        &format!("/rest/getStarred2?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert!(
        listed["starred2"]["song"].as_array().unwrap().is_empty(),
        "the star is gone: {listed}"
    );

    // Ratings.
    let rated = get_json(
        &fixture,
        &format!(
            "/rest/setRating?id={track}&rating=4&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(rated["status"], "ok");
    let song = get_json(
        &fixture,
        &format!("/rest/getSong?id={track}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(song["song"]["userRating"], 4);

    // Playlists.
    let created = get_json(
        &fixture,
        &format!(
            "/rest/createPlaylist?name=Checklist&songId={track}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    let playlist = created["playlist"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["playlist"]["songCount"], 1);
    let updated = get_json(
        &fixture,
        &format!(
            "/rest/updatePlaylist?playlistId={playlist}&name=Checklist%20Two&comment=notes&public=true&songIdToAdd={}&apiKey={}&f=json",
            library.tracks[1], fixture.api_key
        ),
    )
    .await;
    assert_eq!(updated["status"], "ok");
    let listed = get_json(
        &fixture,
        &format!(
            "/rest/getPlaylist?id={playlist}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(listed["playlist"]["name"], "Checklist Two");
    assert_eq!(listed["playlist"]["comment"], "notes");
    assert_eq!(listed["playlist"]["public"], true);
    assert_eq!(listed["playlist"]["songCount"], 2);

    // Play queues, in both shapes.
    let saved = get_json(
        &fixture,
        &format!(
            "/rest/savePlayQueue?id={}&id={}&current={}&position=1500&c=checklist&apiKey={}&f=json",
            library.tracks[0], library.tracks[1], library.tracks[1], fixture.api_key
        ),
    )
    .await;
    assert_eq!(saved["status"], "ok");
    let queue = get_json(
        &fixture,
        &format!("/rest/getPlayQueue?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(queue["playQueue"]["current"], library.tracks[1].to_string());
    assert_eq!(queue["playQueue"]["position"], 1500);
    assert_eq!(queue["playQueue"]["changedBy"], "checklist");
    let indexed = get_json(
        &fixture,
        &format!(
            "/rest/savePlayQueueByIndex?id={}&id={}&currentIndex=0&position=500&c=checklist&apiKey={}&f=json",
            library.tracks[0], library.tracks[1], fixture.api_key
        ),
    )
    .await;
    assert_eq!(indexed["status"], "ok");
    let queue = get_json(
        &fixture,
        &format!(
            "/rest/getPlayQueueByIndex?apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(queue["playQueueByIndex"]["currentIndex"], 0);
    assert_eq!(queue["playQueueByIndex"]["position"], 500);

    // Bookmarks.
    let bookmarked = get_json(
        &fixture,
        &format!(
            "/rest/createBookmark?id={track}&position=1200&comment=here&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(bookmarked["status"], "ok");
    let bookmarks = get_json(
        &fixture,
        &format!("/rest/getBookmarks?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(bookmarks["bookmarks"]["bookmark"][0]["position"], 1200);
    assert_eq!(bookmarks["bookmarks"]["bookmark"][0]["comment"], "here");
    let deleted = get_json(
        &fixture,
        &format!(
            "/rest/deleteBookmark?id={track}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(deleted["status"], "ok");
}

/// The reports a player sends are what the listings read back.
#[tokio::test]
async fn playback_reports_are_what_the_listings_read() {
    let fixture = Fixture::new().await;
    let library = seed_library(&fixture).await;
    let track = library.tracks[0];

    // A now-playing report.
    let reported = get_json(
        &fixture,
        &format!(
            "/rest/scrobble?id={track}&submission=false&c=checklist&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(reported["status"], "ok");
    let playing = get_json(
        &fixture,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(
        playing["nowPlaying"]["entry"][0]["id"],
        track.to_string(),
        "{playing}"
    );
    assert_eq!(playing["nowPlaying"]["entry"][0]["username"], USERNAME);

    // A submitted play is a play.
    let submitted = get_json(
        &fixture,
        &format!(
            "/rest/scrobble?id={track}&c=checklist&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(submitted["status"], "ok");
    let song = get_json(
        &fixture,
        &format!("/rest/getSong?id={track}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(song["song"]["playCount"], 1, "{song}");

    // The playback-report extension, and what it does to now playing.
    let starting = get_json(
        &fixture,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=0&state=starting&c=checklist&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(starting["status"], "ok");
    let playing = get_json(
        &fixture,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(playing["nowPlaying"]["entry"][0]["id"], track.to_string());
    let playing = get_json(
        &fixture,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=1500&state=playing&c=checklist&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(playing["status"], "ok");
    let stopped = get_json(
        &fixture,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=3000&state=stopped&c=checklist&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(stopped["status"], "ok");
    let playing = get_json(
        &fixture,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert!(
        playing["nowPlaying"]["entry"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the stopped session is no longer playing: {playing}"
    );
}

/// A download, a transcode decision, and the extension list.
#[tokio::test]
async fn the_binary_and_administration_endpoints_answer() {
    let fixture = Fixture::new().await;
    let library = seed_library(&fixture).await;
    let track = library.tracks[0];

    let download = fixture
        .get(&format!(
            "/rest/download?id={track}&apiKey={}",
            fixture.api_key
        ))
        .await;
    assert_eq!(download.status(), StatusCode::OK);
    let disposition = download.headers()[CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        disposition.starts_with("attachment; filename=\"tr_"),
        "{disposition}"
    );
    assert!(!download.bytes().await.unwrap().is_empty());

    // The decision is a POST with the client's description.
    let decision = fixture
        .client
        .post(format!(
            "{}/rest/getTranscodeDecision?mediaId={track}&mediaType=song&apiKey={}&f=json",
            fixture.base_url(),
            fixture.api_key
        ))
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "name": "checklist",
                "directPlayProfiles": [
                    { "containers": ["mp3"], "audioCodecs": ["mp3"], "protocols": ["http"] }
                ],
                "transcodingProfiles": [
                    { "container": "mp3", "audioCodec": "mp3", "protocol": "http" }
                ],
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(decision.status(), StatusCode::OK);
    let text = decision.text().await.unwrap();
    let envelope: Value = serde_json::from_str(&text).unwrap();
    let decision = &envelope["subsonic-response"];
    assert_eq!(
        decision["transcodeDecision"]["canDirectPlay"], true,
        "the decision answers: {text}"
    );
    let params = decision["transcodeDecision"]["transcodeParams"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(params, "direct");

    let streamed = fixture
        .get(&format!(
            "/rest/getTranscodeStream?mediaId={track}&mediaType=song&transcodeParams={params}&apiKey={}",
            fixture.api_key
        ))
        .await;
    assert_eq!(streamed.status(), StatusCode::OK);
    assert!(!streamed.bytes().await.unwrap().is_empty());

    let extensions = get_json(
        &fixture,
        &format!(
            "/rest/getOpenSubsonicExtensions?apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    let mut names: Vec<&str> = extensions["openSubsonicExtensions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|extension| extension["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "apiKeyAuthentication",
            "formPost",
            "indexBasedQueue",
            "playbackReport",
            "topSongsByArtistId",
            "transcodeOffset",
            "transcoding",
        ],
        "the advertised extensions are exactly the ones this server implements"
    );

    // The status the scan left behind counts the folder's files.
    let status = get_json(
        &fixture,
        &format!("/rest/getScanStatus?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_eq!(status["scanStatus"]["scanning"], false);
    assert_eq!(status["scanStatus"]["folderCount"], 1);
    assert_eq!(status["scanStatus"]["count"], TITLES.len());

    let user = get_json(
        &fixture,
        &format!(
            "/rest/getUser?username={USERNAME}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(user["user"]["username"], USERNAME);
    assert_eq!(user["user"]["adminRole"], true);
}

/// The checklist runs over one library folder and one account.
#[tokio::test]
async fn the_seeded_library_is_one_folder_of_two_files() {
    let fixture = Fixture::new().await;
    let library = seed_library(&fixture).await;

    assert!(
        Path::new(&library.folder).is_dir(),
        "the scanned folder is where the test wrote it"
    );
    assert_eq!(library.tracks.len(), TITLES.len());
}
