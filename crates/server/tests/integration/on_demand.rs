//! Full-stack tests of on-demand playback.
//!
//! Every test builds the real server over a temporary data directory, so a
//! request travels the whole path a client's request travels: `/rest` over real
//! HTTP, the Subsonic authentication, the catalog layer over the fixture
//! plugins, the acquisition ledger, the simulated download, the import into
//! managed storage, and the bytes a player receives. The catalog fixture
//! answers one album of five tracks, and the acquisition fixture serves the
//! generated audio a test places in the import root it is granted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::common::Fixture;
use hearthsound_acquire::orchestrator::{AcquireRequest, AcquisitionOrchestrator};
use hearthsound_acquire::repo::{AcquisitionRow, ListFilter};
use hearthsound_catalog::CatalogService;
use hearthsound_config::{SettingValue, keys};
use hearthsound_core::{AcquisitionState, Availability, EntityId, EntityKind, Layer};
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, TrackInput};
use hearthsound_media::{Container, probe};
use hearthsound_plugin_host::{InstanceSpec, PluginRepository, PluginSource};
use hearthsound_server::Services;
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
use hearthsound_test_support::{
    ACQUISITION_FIXTURE, ALBUM_CATALOG, FAKE_YT_DLP, PROCESS_FIXTURE, fixture_plugin, fixture_tool,
};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT_RANGES, CONTENT_RANGE, CONTENT_TYPE, RANGE, RETRY_AFTER};
use serde_json::{Value, json};

/// The manifest id of the album catalog fixture.
const ALBUM_PLUGIN: &str = "com.example.album-catalog";

/// The manifest id of the acquisition fixture.
const ACQ_PLUGIN: &str = "com.example.acquisition-fixture";

/// The slug of the album catalog instance.
const CATALOG_SLUG: &str = "album";

/// The slug of the acquisition instance.
const ACQ_SLUG: &str = "acq";

/// The title of the fixture's first track.
const FIRST_TITLE: &str = "Fixture Song 1";

/// The file name the fixture's first download produces.
const FIRST_FILE: &str = "01 - Fixture Song 1.flac";

/// The path the first track is imported to, relative to the cache root.
pub(crate) const FIRST_REL_PATH: &str =
    "Fixture Artist/Fixture Album (2026)/01 - Fixture Song 1.flac";

/// How long the generated audio lasts, in seconds.
///
/// The catalog fixture's tracks claim 180 seconds, and verification refuses a
/// file whose duration is more than five seconds away from the track's.
const AUDIO_SECONDS: f64 = 180.0;

/// The duration of the fixture's tracks, in milliseconds.
const TRACK_MS: u64 = 180_000;

/// The International Standard Recording Code the first track is given.
///
/// The catalog fixture's own tracks carry none, so the test writes the field a
/// provider would have filled in before the import reads it.
pub(crate) const FIRST_ISRC: &str = "USHW12600001";

/// Installs the album catalog fixture and assigns it to the catalog layer.
///
/// # Parameters
///
/// - `services`: the running services.
pub(crate) async fn install_catalog(services: &Arc<Services>) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(ALBUM_CATALOG))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, ALBUM_PLUGIN);
    services
        .instances()
        .create(&InstanceSpec {
            slug: CATALOG_SLUG.to_owned(),
            plugin_id: ALBUM_PLUGIN.to_owned(),
            display_name: "Album catalog".to_owned(),
            server_config: json!({}),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    services
        .instances()
        .set_layer(Layer::Catalog, &[CATALOG_SLUG.to_owned()])
        .await
        .unwrap();
}

/// Installs the acquisition fixture and assigns it to the acquisition layer.
///
/// # Parameters
///
/// - `services`: the running services.
/// - `downloads`: the directory granted as the instance's `downloads` import
///   root, in `move` mode, which is where a completed download leaves its file.
/// - `scenario`: what the simulated backend does, `normal`, `no-candidates`, or
///   `unavailable` (every call answers that the backend is not answering, so
///   an admitted acquisition parks as `source-unavailable`).
/// - `running_polls`: how many polls the download reports progress before it
///   completes; the polls are two seconds apart.
pub(crate) async fn install_acquisition(
    services: &Arc<Services>,
    downloads: &Path,
    scenario: &str,
    running_polls: u32,
) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(ACQUISITION_FIXTURE))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, ACQ_PLUGIN);
    services
        .instances()
        .create(&InstanceSpec {
            slug: ACQ_SLUG.to_owned(),
            plugin_id: ACQ_PLUGIN.to_owned(),
            display_name: "Acquisition fixture".to_owned(),
            server_config: json!({
                "candidates": [candidate()],
                "scenario": scenario,
                "searchPolls": 0,
                "downloadQueuedPolls": 0,
                "downloadRunningPolls": running_polls,
                "downloadBytesPerPoll": 4000,
                "outputRoot": "downloads",
                "matchSuffix": ".flac",
            }),
            host_config: json!({
                "importRoots": [{
                    "id": "downloads",
                    "path": downloads.to_string_lossy(),
                    "mode": "move",
                    "readOnly": false,
                }],
                "maxConcurrent": 2,
                "maxConcurrentSearches": 2,
                "searchesPerHour": 120,
            }),
        })
        .await
        .unwrap();
    services
        .instances()
        .set_layer(Layer::Acquisition, &[ACQ_SLUG.to_owned()])
        .await
        .unwrap();
}

/// The one candidate the simulated backend offers for the first track.
///
/// The path carries the artist, the album and the track number, so scoring
/// recovers all three from it the way it would from a real backend's share.
fn candidate() -> Value {
    json!({
        "token": "cand-1",
        "title": FIRST_TITLE,
        "path": "Fixture Artist/Fixture Album/01 - Fixture Song 1.flac",
        "files": [{
            "name": FIRST_FILE,
            "size": 4000,
            "format": "flac",
            "bitrateKbps": 900,
            "sampleRate": 44100,
            "bitDepth": 16,
            "durationMs": TRACK_MS,
            "trackIndex": 0,
        }],
        "source": { "key": "peer-1", "speedBps": 1250000, "queueLength": 0, "freeSlot": true },
    })
}

/// Resolves the fixture's album through the search surface and returns its row.
///
/// # Parameters
///
/// - `catalog`: the running catalog layer.
///
/// # Panics
///
/// Panics when the fixture answers no album, which means the layer is not
/// installed or not assigned to the catalog layer.
pub(crate) async fn catalog_album(catalog: &CatalogService) -> EntityId {
    catalog
        .search(uuid::Uuid::nil(), "Fixture", &[EntityKind::Album], 5, 0)
        .await
        .unwrap()
        .albums
        .into_iter()
        .next()
        .expect("the fixture answers an album")
        .album
        .id
}

/// Resolves one track of the fixture's album to its stored row.
///
/// The album is found through search and its page walked, which is how a client
/// reaches the same rows: the album page resolves the provider's whole
/// tracklist, so every one of its tracks has a row before the caller plays it.
///
/// # Parameters
///
/// - `catalog`: the running catalog layer.
/// - `number`: the track number within the album, one-based.
///
/// # Panics
///
/// Panics when the album page holds no track with that number.
pub(crate) async fn resolve_track(catalog: &CatalogService, number: u8) -> EntityId {
    let album = catalog_album(catalog).await;
    let page = catalog
        .views()
        .album(
            album,
            hearthsound_catalog::views::Viewer {
                user: uuid::Uuid::nil(),
                vendor: false,
            },
        )
        .await
        .unwrap()
        .expect("the resolved album has a page");
    page.tracks
        .into_iter()
        .find(|entry| entry.track.track_number == Some(i64::from(number)))
        .unwrap_or_else(|| panic!("the album page holds track {number}"))
        .track
        .id
}

/// Creates the instance's import root inside the fixture's data directory.
///
/// The directory sits beside the database, so it is the same directory tree a
/// restarted process finds again.
pub(crate) fn downloads_dir(fixture: &Fixture) -> PathBuf {
    let downloads = fixture.database_path().parent().unwrap().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    downloads
}

/// Writes the audio the simulated backend serves into its import root.
///
/// The file is generated and tagged, and its tags name another track on purpose:
/// the imported file has to carry the tags of the track it was acquired for, not
/// the ones the backend's file happened to hold.
///
/// # Parameters
///
/// - `downloads`: the instance's import root.
///
/// # Returns
///
/// The path of the written file.
pub(crate) async fn place_audio(downloads: &Path) -> PathBuf {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(AUDIO_SECONDS)
        .file_name(FIRST_FILE.to_owned())
        .tags(
            TrackTags::new("Someone Else's Song", "Someone Else")
                .album("Another Album")
                .isrc("AAAAAAAAAAAA"),
        )
        .write_into(downloads)
        .await
        .unwrap()
}

/// Gives a stored track an International Standard Recording Code.
///
/// The catalog fixture's tracks carry none, but an imported file has to keep the
/// code of the track it was acquired for, so the test writes the column a
/// provider's own record would have filled in.
///
/// # Parameters
///
/// - `services`: the running services.
/// - `track`: the stored track to write.
/// - `isrc`: the code to store.
pub(crate) async fn give_track_isrc(services: &Arc<Services>, track: EntityId, isrc: &str) {
    let mut connection = services.database().writer().acquire().await.unwrap();
    hearthsound_library::external_ids::attach(
        &mut connection,
        EntityKind::Track,
        track.uuid(),
        &[hearthsound_core::providers::ExternalId {
            namespace: hearthsound_core::providers::ISRC.to_owned(),
            value: isrc.to_owned(),
        }],
    )
    .await
    .unwrap();
}

/// Returns the path of a `stream` request for one id.
///
/// # Parameters
///
/// - `fixture`: the running fixture, for its api key.
/// - `id`: the track id to stream.
fn stream_path(fixture: &Fixture, id: &str) -> String {
    format!("/rest/stream?id={id}&apiKey={}", fixture.api_key)
}

/// Plays one remote track and returns the bytes the client received.
///
/// The answer is the held pending stream: nothing is sent while the acquisition
/// runs, and once it has imported the file the response is the stored file
/// itself - `200 audio/flac` with a length and byte ranges. Nothing is written
/// before then, so the body is exactly the file.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `id`: the track id to stream.
///
/// # Returns
///
/// The whole body, which is the imported file.
pub(crate) async fn play_stream(fixture: &Fixture, id: &str) -> Vec<u8> {
    let response = fixture.get(&stream_path(fixture, id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, CONTENT_TYPE),
        Some("audio/flac"),
        "a held stream is served as the stored file is"
    );
    assert_eq!(
        header(&response, ACCEPT_RANGES),
        Some("bytes"),
        "a held stream can be seeked once its file exists"
    );
    read_body(response, Duration::from_secs(120)).await
}

/// Reads a response body to its end.
///
/// # Parameters
///
/// - `response`: the response to drain.
/// - `deadline`: how long one chunk may take.
///
/// # Returns
///
/// The bytes of the body.
async fn read_body(mut response: reqwest::Response, deadline: Duration) -> Vec<u8> {
    let mut body = Vec::new();
    while let Some(chunk) = tokio::time::timeout(deadline, response.chunk())
        .await
        .expect("the stream kept producing bytes")
        .unwrap()
    {
        body.extend_from_slice(&chunk);
    }
    body
}

/// Returns one response header as text.
///
/// # Parameters
///
/// - `response`: the response to read.
/// - `name`: the header to read.
fn header(response: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<&str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

/// Returns the ledger rows of one track.
///
/// # Parameters
///
/// - `orchestrator`: the running acquisition layer.
/// - `track`: the track to read.
pub(crate) async fn ledger_rows(
    orchestrator: &AcquisitionOrchestrator,
    track: EntityId,
) -> Vec<AcquisitionRow> {
    let listed = orchestrator.list(&ListFilter::default()).await.unwrap();
    let mut rows = Vec::new();
    for summary in listed.into_iter().filter(|row| row.track_id == Some(track)) {
        rows.push(
            orchestrator
                .repository()
                .get(summary.id)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    rows
}

/// Waits until an acquisition reaches a state, up to thirty seconds.
///
/// # Parameters
///
/// - `orchestrator`: the running acquisition layer.
/// - `id`: the acquisition to watch.
/// - `state`: the state it has to reach.
///
/// # Panics
///
/// Panics when the state is not reached in time, so a stuck pipeline fails the
/// test instead of hanging it.
async fn wait_for_state(
    orchestrator: &AcquisitionOrchestrator,
    id: uuid::Uuid,
    state: AcquisitionState,
) -> AcquisitionRow {
    for _ in 0..1500 {
        let row = orchestrator.repository().get(id).await.unwrap().unwrap();
        if row.state == state {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let row = orchestrator.repository().get(id).await.unwrap().unwrap();
    panic!(
        "acquisition {id} stayed in {} instead of reaching {state}",
        row.state
    );
}

/// Waits until a condition holds, up to thirty seconds.
///
/// # Parameters
///
/// - `condition`: the condition to poll.
///
/// # Panics
///
/// Panics when the condition never holds.
pub(crate) async fn eventual<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..1500 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the condition never held");
}

/// Waits until a track has an acquisition that is downloading and has recorded
/// the backend's download token.
///
/// The ledger moves to `downloading` before the backend's answer is written, so
/// the token is what proves a download is in flight and can be resumed.
///
/// # Parameters
///
/// - `orchestrator`: the running acquisition layer.
/// - `track`: the track being acquired.
///
/// # Panics
///
/// Panics when no acquisition of the track reaches `downloading` with a token.
async fn wait_for_download(
    orchestrator: &AcquisitionOrchestrator,
    track: EntityId,
) -> AcquisitionRow {
    for _ in 0..1500 {
        if let Some(row) = ledger_rows(orchestrator, track)
            .await
            .into_iter()
            .find(|row| row.state == AcquisitionState::Downloading && row.plugin_token.is_some())
        {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no acquisition of {track} reached the download");
}

/// `search3` answers the local library first and the catalog after it.
#[tokio::test]
async fn search3_lists_local_results_before_catalog_results() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();

    // One track the server already stores, which the query matches.
    let library = LibraryRepository::new(services.database());
    let album = library
        .upsert_album(
            &AlbumInput::new("Fixture Local Album"),
            &[ArtistCredit::new("Fixture Local Artist", "main")],
        )
        .await
        .unwrap();
    let mut input = TrackInput::new("Fixture Local Song", Availability::Local);
    input.album_id = Some(album);
    input.display_artist = Some("Fixture Local Artist".to_owned());
    input.track_number = Some(1);
    let local = library
        .insert_track(
            &input,
            &[ArtistCredit::new("Fixture Local Artist", "main")],
            &[],
        )
        .await
        .unwrap();
    // The default browse scope is the requester's personal library, so the
    // track has to be in it for the search to see it at all.
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'saved', 0)",
    )
    .bind(fixture.user_id.as_bytes().as_slice())
    .bind(local.uuid().as_bytes().to_vec())
    .execute(services.database().writer())
    .await
    .unwrap();

    let response = fixture
        .get(&format!(
            "/rest/search3?query=Fixture&apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let result = &body["subsonic-response"]["searchResult3"];

    // Every hit is a stored row, so the order is the only thing that tells the
    // caller's own library from what the catalog layer contributed: the local
    // row of each kind comes first and the provider's rows follow it.
    for (kind, field) in [("artist", "name"), ("album", "name")] {
        let entries = result[kind].as_array().unwrap();
        assert!(
            entries.len() >= 2,
            "`{kind}` has a catalog result: {entries:?}"
        );
        assert!(
            entries[0][field]
                .as_str()
                .unwrap()
                .starts_with("Fixture Local"),
            "`{kind}` lists the local row first: {entries:?}"
        );
        assert!(
            entries[1..]
                .iter()
                .all(|entry| !entry[field].as_str().unwrap().starts_with("Fixture Local")),
            "no local row of `{kind}` follows a catalog one: {entries:?}"
        );
    }

    let songs = result["song"].as_array().unwrap();
    assert_eq!(
        songs[0]["id"],
        local.to_string(),
        "the local track comes first"
    );
    assert_eq!(
        songs[1]["title"], "Fixture Song 1",
        "the catalog's own track follows it: {songs:?}"
    );
    assert_ne!(
        songs[1]["id"],
        local.to_string(),
        "the catalog hit is its own stored row"
    );
}

/// A catalog stream is held and then serves the imported file.
#[tokio::test]
async fn a_catalog_stream_serves_the_imported_file() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    let source = place_audio(&downloads).await;

    let track = resolve_track(services.catalog(), 1).await;
    give_track_isrc(&services, track, FIRST_ISRC).await;
    let id = track.to_string();

    let body = play_stream(&fixture, &id).await;

    // The body is the imported file byte for byte: the held stream sent nothing
    // before the file existed and then served it as any stored file is served.
    let imported = services.config().paths.cache.join(FIRST_REL_PATH);
    assert_eq!(
        body,
        std::fs::read(&imported).unwrap(),
        "the held stream carried the whole imported file"
    );

    // The file landed in the cache under the name the template renders, tagged
    // with the track's own metadata rather than with the tags the backend's file
    // carried, and the import root no longer holds it.
    let probed = probe(&imported).await.unwrap();
    assert_eq!(probed.container, Container::Flac);
    assert!(probed.lossless);
    assert_eq!(probed.tags.title.as_deref(), Some(FIRST_TITLE));
    assert_eq!(probed.tags.artists, ["Fixture Artist"]);
    assert_eq!(probed.tags.album.as_deref(), Some("Fixture Album"));
    assert_eq!(probed.tags.album_artist.as_deref(), Some("Fixture Artist"));
    assert_eq!(probed.tags.track_number, Some(1));
    assert_eq!(probed.tags.date.as_deref(), Some("2026-02-01"));
    assert_eq!(probed.tags.isrc.as_deref(), Some(FIRST_ISRC));
    assert!(
        !source.exists(),
        "a move import root drops the file it served"
    );
    assert_eq!(
        LibraryRepository::new(services.database())
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Cached
    );
}

/// Replaying an imported track serves the stored file with a range.
#[tokio::test]
async fn a_replay_of_an_imported_track_serves_a_range() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    let track = resolve_track(services.catalog(), 1).await;
    let id = track.to_string();
    play_stream(&fixture, &id).await;

    let imported = services.config().paths.cache.join(FIRST_REL_PATH);
    assert!(imported.is_file(), "the stream imported its file");
    let size = std::fs::metadata(&imported).unwrap().len();
    assert_eq!(
        LibraryRepository::new(services.database())
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Cached
    );

    // The same id now resolves to the stored file, which a seeking client reads
    // in parts.
    let response = fixture
        .client
        .get(format!(
            "{}{}",
            fixture.base_url(),
            stream_path(&fixture, &id)
        ))
        .header(RANGE, "bytes=0-1023")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        header(&response, CONTENT_RANGE),
        Some(format!("bytes 0-1023/{size}").as_str()),
        "the answer names the part of the file it carries"
    );
    assert_eq!(response.bytes().await.unwrap().len(), 1024);
}

/// A track with no sources ends its stream and cools down.
#[tokio::test]
async fn a_track_with_no_sources_ends_its_stream_and_cools_down() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "no-candidates", 0).await;
    services.registry().rebuild().await.unwrap();

    let track = resolve_track(services.catalog(), 1).await;
    let id = track.to_string();
    let response = fixture.get(&stream_path(&fixture, &id)).await;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a held stream ends as 404 when its acquisition fails"
    );
    assert!(
        response.bytes().await.unwrap().is_empty(),
        "a failed acquisition sends no bytes"
    );

    let orchestrator = Arc::clone(services.orchestrator());
    eventual(|| {
        let orchestrator = Arc::clone(&orchestrator);
        async move {
            ledger_rows(&orchestrator, track)
                .await
                .iter()
                .any(|row| row.state == AcquisitionState::Failed)
        }
    })
    .await;
    let rows = ledger_rows(&orchestrator, track).await;
    assert_eq!(rows.len(), 1, "the search ran once");
    assert_eq!(
        rows[0].failure_reason,
        Some(hearthsound_core::FailureReason::NoCandidates)
    );
    assert_eq!(
        LibraryRepository::new(services.database())
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Unavailable,
        "a track no backend can find cools down"
    );

    // The cooldown refuses the replay, and refusing it starts no new search.
    let replay = fixture.get(&stream_path(&fixture, &id)).await;
    assert_eq!(replay.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        ledger_rows(&orchestrator, track).await.len(),
        1,
        "the cooldown started no second acquisition"
    );
}

/// `stream.pending.mode` switches between holding and failing.
#[tokio::test]
async fn the_pending_mode_switches_between_hold_and_fail() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    let first = resolve_track(services.catalog(), 1).await;
    let second = resolve_track(services.catalog(), 2).await;

    // `hold` sends nothing until the file is there, then serves it as any stored
    // track is served.
    services
        .settings()
        .set(
            keys::STREAM_PENDING_MODE.name(),
            SettingValue::String("hold".to_owned()),
            None,
        )
        .await
        .unwrap();
    let response = fixture
        .client
        .get(format!(
            "{}{}",
            fixture.base_url(),
            stream_path(&fixture, &first.to_string())
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, CONTENT_TYPE), Some("audio/flac"));
    let body = response.bytes().await.unwrap();
    let imported = services.config().paths.cache.join(FIRST_REL_PATH);
    assert_eq!(
        body.len() as u64,
        std::fs::metadata(&imported).unwrap().len(),
        "the held stream carried the whole imported file"
    );

    // `fail` answers at once with a retry hint, while the acquisition a client
    // asked for runs on in the background.
    services
        .settings()
        .set(
            keys::STREAM_PENDING_MODE.name(),
            SettingValue::String("fail".to_owned()),
            None,
        )
        .await
        .unwrap();
    let response = fixture
        .get(&stream_path(&fixture, &second.to_string()))
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, RETRY_AFTER), Some("10"));
}

/// A stream whose acquisition waits for a source answers at once.
///
/// The catalog layer is installed and the acquisition layer is empty, so there
/// is no instance that could serve the track: admission parks the acquisition
/// instead of starting it, and the stream answers `503` with `Retry-After`
/// rather than holding the client's connection for
/// `stream.pending.max_wait_secs`.
#[tokio::test]
async fn a_stream_no_source_can_serve_answers_at_once() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    install_catalog(&services).await;
    services.registry().rebuild().await.unwrap();

    let track = resolve_track(services.catalog(), 1).await;
    let id = track.to_string();
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.get(&stream_path(&fixture, &id)),
    )
    .await
    .expect("a stream waiting for a source is answered without holding the connection");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        header(&response, RETRY_AFTER),
        Some("10"),
        "a client is told when to ask again"
    );
    assert!(
        response.bytes().await.unwrap().is_empty(),
        "a parked stream sends no bytes"
    );

    let rows = ledger_rows(services.orchestrator(), track).await;
    assert_eq!(rows.len(), 1, "the request started one acquisition");
    assert_eq!(rows[0].state, AcquisitionState::Queued);
    assert_eq!(
        rows[0].park_reason,
        Some(hearthsound_core::ParkReason::NoSource),
        "the acquisition waits for an acquisition source"
    );
}

/// A stream whose acquisition parks after the hold began answers at once.
///
/// The acquisition source is installed but does not answer, so the acquisition
/// parks as `source-unavailable` a moment after the request was admitted - that
/// is, while `/rest/stream` is already holding the client. A held stream must
/// answer `503` with `Retry-After` as soon as the park happens, not when
/// `stream.pending.max_wait_secs` runs out.
#[tokio::test]
async fn a_stream_whose_acquisition_parks_during_the_hold_answers_at_once() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "unavailable", 0).await;
    services.registry().rebuild().await.unwrap();
    // The full wait is the value a server answers this defect with: a park the
    // hold misses leaves the client waiting this long.
    services
        .settings()
        .set(
            keys::STREAM_PENDING_MAX_WAIT_SECS.name(),
            SettingValue::Integer(180),
            None,
        )
        .await
        .unwrap();

    let track = resolve_track(services.catalog(), 1).await;
    let id = track.to_string();
    let started = std::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_secs(15),
        fixture.get(&stream_path(&fixture, &id)),
    )
    .await
    .expect("a stream whose acquisition parked is answered without waiting out its bound");
    let elapsed = started.elapsed();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a parked acquisition is not a failed stream"
    );
    assert_eq!(
        header(&response, RETRY_AFTER),
        Some("10"),
        "a client is told when to ask again"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "the hold noticed the park instead of waiting out `stream.pending.max_wait_secs`: {elapsed:?}"
    );
    assert!(
        response.bytes().await.unwrap().is_empty(),
        "a parked stream sends no bytes"
    );

    let rows = ledger_rows(services.orchestrator(), track).await;
    assert_eq!(rows.len(), 1, "the request started one acquisition");
    assert_eq!(
        rows[0].state,
        AcquisitionState::Queued,
        "a park leaves the acquisition queued for a source"
    );
    assert_eq!(
        rows[0].park_reason,
        Some(hearthsound_core::ParkReason::SourceUnavailable),
        "the acquisition waits because its only source does not answer"
    );
}

/// A restart in the middle of a download finishes the acquisition.
#[tokio::test]
async fn a_restart_mid_download_finishes_the_acquisition() {
    // The first process runs on its own thread so that its runtime can be
    // dropped, which is what stopping the server looks like to the ledger: the
    // download stays in flight with its token in the row and the backend's own
    // state in `plugin_kv`.
    let (dir, api_key, id, track) = tokio::task::spawn_blocking(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fixture = Fixture::new().await;
            let services = Arc::clone(&fixture.services);
            let downloads = downloads_dir(&fixture);
            install_catalog(&services).await;
            // Forty polls 50 ms apart: long enough to be stopped in the middle
            // of the download, short enough for the next process to finish it.
            install_acquisition(&services, &downloads, "normal", 40).await;
            services.registry().rebuild().await.unwrap();
            place_audio(&downloads).await;

            // `fail` answers the request at once and leaves the acquisition it
            // started running, which is what this test needs: a download in
            // flight while the process stops.
            services
                .settings()
                .set(
                    keys::STREAM_PENDING_MODE.name(),
                    SettingValue::String("fail".to_owned()),
                    None,
                )
                .await
                .unwrap();
            let track = resolve_track(services.catalog(), 1).await;
            let response = fixture
                .get(&stream_path(&fixture, &track.to_string()))
                .await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(header(&response, RETRY_AFTER), Some("10"));

            let row = wait_for_download(services.orchestrator(), track).await;

            let api_key = fixture.api_key.clone();
            let dir = fixture.stop().await;
            (dir, api_key, row.id, track)
        })
    })
    .await
    .unwrap();

    // The second process finds the same database, the same storage and the same
    // fixture backend, and resumes what the first one left in flight.
    let fixture = Fixture::reopen(dir, api_key).await;
    let services = Arc::clone(&fixture.services);
    let row = wait_for_state(services.orchestrator(), id, AcquisitionState::Completed).await;
    assert!(
        row.media_file_id.is_some(),
        "the resumed import stored a file"
    );
    assert_eq!(row.failure_reason, None);

    let imported = services.config().paths.cache.join(FIRST_REL_PATH);
    let probed = probe(&imported).await.unwrap();
    assert_eq!(probed.tags.title.as_deref(), Some(FIRST_TITLE));
    assert_eq!(probed.tags.album.as_deref(), Some("Fixture Album"));
    assert!(
        !downloads_dir(&fixture).join(FIRST_FILE).exists(),
        "the resumed import emptied the import root"
    );
    assert_eq!(
        LibraryRepository::new(services.database())
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Cached
    );
}

/// Finds the fixture album's stored row through `search3`.
///
/// The search resolves the provider's album hit to a row, which is the id every
/// later request names.
pub(crate) async fn catalog_album_id(fixture: &Fixture) -> String {
    let response = fixture
        .get(&format!(
            "/rest/search3?query=Fixture&apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    body["subsonic-response"]["searchResult3"]["album"]
        .as_array()
        .unwrap()
        .iter()
        .map(|album| album["id"].as_str().unwrap().to_owned())
        .next()
        .expect("the catalog answers the fixture album")
}

/// Stars ids through `/rest/star` and expects an ok envelope.
pub(crate) async fn star(fixture: &Fixture, parameter: &str, id: &str) {
    let response = fixture
        .get(&format!(
            "/rest/star?{parameter}={id}&apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(text.contains("\"status\":\"ok\""), "{text}");
}

/// Liking a catalog album acquires its tracks at priority `queue`, and what the
/// backend serves lands in the permanent library root.
#[tokio::test]
async fn starring_a_catalog_album_acquires_it_into_the_library() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    // The client opens the album before it likes it, which is what stores the
    // provider's tracklist; the like then adopts every track of the album.
    let album = crate::on_demand::catalog_album(services.catalog()).await;
    services
        .catalog()
        .ensure_album_page(album, Duration::from_secs(5))
        .await
        .unwrap();
    star(&fixture, "albumId", &album.to_string()).await;

    let mut tracks = Vec::new();
    for number in 1..=5 {
        let track = resolve_track(services.catalog(), number).await;
        let rows = ledger_rows(services.orchestrator(), track).await;
        assert_eq!(rows.len(), 1, "one acquisition per album track");
        assert_eq!(rows[0].priority, hearthsound_core::Priority::Queue);
        assert_eq!(rows[0].reason, "save");
        assert_eq!(rows[0].requested_by, Some(fixture.user_id));
        tracks.push(track);
    }

    // The backend holds one file, which the closest-scoring track's
    // acquisition imports; the album's tracks are pinned by the like, so the
    // import lands in the library root rather than the cache.
    let library = LibraryRepository::new(services.database());
    let mut imported = None;
    for _ in 0..1500 {
        for track in &tracks {
            let files = library.list_media_files_for_track(*track).await.unwrap();
            if let Some(file) = files.into_iter().next() {
                imported = Some((*track, file));
            }
        }
        if imported.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (track, file) = imported.expect("one album track is imported");
    assert_eq!(file.storage, hearthsound_core::StorageClass::Library);
    assert!(
        services
            .config()
            .paths
            .library
            .join(&file.rel_path)
            .is_file()
    );
    assert_eq!(
        library
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Local
    );
}

/// A like without the acquire permission, or with acquire-on-save turned off,
/// is recorded and acquires nothing.
#[tokio::test]
async fn a_like_acquires_nothing_without_permission_or_with_the_setting_off() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();

    services
        .settings()
        .set(
            keys::LIBRARY_ACQUIRE_ON_SAVE.name(),
            SettingValue::Bool(false),
            None,
        )
        .await
        .unwrap();
    let first = resolve_track(services.catalog(), 1).await;
    star(&fixture, "id", &first.to_string()).await;
    assert!(ledger_rows(services.orchestrator(), first).await.is_empty());

    services
        .settings()
        .unset(keys::LIBRARY_ACQUIRE_ON_SAVE.name())
        .await
        .unwrap();
    hearthsound_auth::Users::new(services.database())
        .update(
            fixture.user_id,
            &hearthsound_auth::UserUpdate::default().with_permissions(
                hearthsound_auth::UserPermissions {
                    acquire: false,
                    upload: true,
                    download: true,
                },
            ),
        )
        .await
        .unwrap();
    let second = resolve_track(services.catalog(), 2).await;
    star(&fixture, "id", &second.to_string()).await;
    assert!(
        ledger_rows(services.orchestrator(), second)
            .await
            .is_empty()
    );

    // Both likes are recorded and pin their tracks all the same.
    let library = LibraryRepository::new(services.database());
    assert!(library.track_is_pinned(first).await.unwrap());
    assert!(library.track_is_pinned(second).await.unwrap());
}

/// Adding a catalog track to a playlist acquires it into the library root, and
/// turning acquire-on-playlist-add off leaves a later addition remote.
#[tokio::test]
async fn adding_a_catalog_track_to_a_playlist_acquires_it() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    let first = resolve_track(services.catalog(), 1).await;
    let response = fixture
        .get(&format!(
            "/rest/createPlaylist?name=Mix&songId={}&apiKey={}&f=json",
            first, fixture.api_key
        ))
        .await;
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let playlist = body["subsonic-response"]["playlist"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let rows = ledger_rows(services.orchestrator(), first).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason, "playlist");
    assert_eq!(rows[0].priority, hearthsound_core::Priority::Queue);
    wait_for_state(
        services.orchestrator(),
        rows[0].id,
        AcquisitionState::Completed,
    )
    .await;
    assert!(
        services
            .config()
            .paths
            .library
            .join(FIRST_REL_PATH)
            .is_file()
    );

    services
        .settings()
        .set(
            keys::LIBRARY_ACQUIRE_ON_PLAYLIST_ADD.name(),
            SettingValue::Bool(false),
            None,
        )
        .await
        .unwrap();
    let second = resolve_track(services.catalog(), 2).await;
    let response = fixture
        .get(&format!(
            "/rest/updatePlaylist?playlistId={playlist}&songIdToAdd={}&apiKey={}&f=json",
            second, fixture.api_key
        ))
        .await;
    assert!(response.text().await.unwrap().contains("\"status\":\"ok\""));
    assert!(
        ledger_rows(services.orchestrator(), second)
            .await
            .is_empty()
    );
}

/// The manifest id of the process fixture.
const PROCESS_PLUGIN: &str = "com.example.process-fixture";

/// Installs the process fixture and assigns it to the acquisition layer.
///
/// The plugin starts the `yt-dlp` tool for every download, so the test also
/// points `tools.yt-dlp.path` at the stand-in before the registry is rebuilt.
///
/// # Parameters
///
/// - `services`: the running server's services.
/// - `tool`: the executable to configure as `yt-dlp`, or `None` to leave the
///   setting empty.
async fn install_process_fixture(services: &Arc<Services>, tool: Option<&Path>) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(PROCESS_FIXTURE))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, PROCESS_PLUGIN);
    services
        .instances()
        .create(&InstanceSpec {
            slug: "tool".to_owned(),
            plugin_id: PROCESS_PLUGIN.to_owned(),
            display_name: "Process fixture".to_owned(),
            server_config: json!({}),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    services
        .instances()
        .set_layer(Layer::Acquisition, &["tool".to_owned()])
        .await
        .unwrap();

    let path = tool
        .map(|tool| tool.display().to_string())
        .unwrap_or_default();
    services
        .settings()
        .set(
            keys::TOOLS_YT_DLP_PATH.name(),
            SettingValue::String(path),
            None,
        )
        .await
        .unwrap();
    services.registry().rebuild().await.unwrap();
}

/// A catalog track is downloaded by the configured tool and imported.
#[tokio::test]
async fn a_track_is_acquired_through_a_configured_tool() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    install_catalog(&services).await;
    install_process_fixture(&services, Some(&fixture_tool(FAKE_YT_DLP))).await;

    let track = resolve_track(services.catalog(), 1).await;
    services
        .orchestrator()
        .request(
            AcquireRequest::new(
                track,
                hearthsound_core::Priority::Play,
                "the test plays the track".to_owned(),
            )
            .with_can_acquire(true),
        )
        .await
        .unwrap();
    let mut acquisition = None;
    for _ in 0..1500 {
        if let Some(found) = ledger_rows(services.orchestrator(), track)
            .await
            .into_iter()
            .next()
        {
            acquisition = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let acquisition = acquisition.expect("the request writes an acquisition");
    let mut row = acquisition.clone();
    for _ in 0..1500 {
        row = services
            .orchestrator()
            .repository()
            .get(acquisition.id)
            .await
            .unwrap()
            .unwrap();
        if row.state.is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        row.state,
        AcquisitionState::Completed,
        "state {:?}, detail {:?}, log {:?}",
        row.failure_reason,
        row.error_detail,
        services
            .orchestrator()
            .repository()
            .log(acquisition.id)
            .await
            .unwrap()
    );
    assert_eq!(row.track_id, Some(track));
    let response = fixture
        .get(&stream_path(&fixture, &track.to_string()))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let (format, codec, tier): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT format, codec, quality_tier FROM media_files WHERE track_id = ?")
            .bind(track.uuid().as_bytes().to_vec())
            .fetch_one(services.database().writer())
            .await
            .unwrap();
    assert_eq!(format.as_deref(), Some("ogg"));
    assert_eq!(codec.as_deref(), Some("opus"));
    assert_eq!(tier.as_deref(), Some("lossy-high"));
}

/// An acquisition whose tool has no configured path fails and names the reason.
#[tokio::test]
async fn an_unconfigured_tool_fails_the_acquisition() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    install_catalog(&services).await;
    install_process_fixture(&services, None).await;

    let track = resolve_track(services.catalog(), 1).await;
    services
        .orchestrator()
        .request(
            AcquireRequest::new(
                track,
                hearthsound_core::Priority::Background,
                "the test asks for the track".to_owned(),
            )
            .with_can_acquire(true),
        )
        .await
        .unwrap();

    let mut acquisition = None;
    for _ in 0..1500 {
        if let Some(found) = ledger_rows(services.orchestrator(), track)
            .await
            .into_iter()
            .next()
        {
            acquisition = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let acquisition = acquisition.expect("the request writes an acquisition");
    let row = wait_for_state(
        services.orchestrator(),
        acquisition.id,
        AcquisitionState::Failed,
    )
    .await;
    let entries = services
        .orchestrator()
        .repository()
        .log(row.id)
        .await
        .unwrap();
    assert!(
        entries.iter().any(|entry| entry
            .message
            .as_deref()
            .is_some_and(|message| message.contains("not-configured"))),
        "the log should name the missing tool: {entries:?}"
    );
}
