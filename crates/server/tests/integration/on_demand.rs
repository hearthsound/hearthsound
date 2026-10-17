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
use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
use hearthsound_acquire::repo::{AcquisitionRow, ListFilter};
use hearthsound_catalog::CatalogService;
use hearthsound_config::{SettingValue, keys};
use hearthsound_core::{
    AcquisitionState, Availability, CatalogId, EntityId, EntityKind, Layer, Slug,
};
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin, TrackInput};
use hearthsound_media::{Container, probe};
use hearthsound_plugin_host::{InstanceSpec, PluginRepository, PluginSource};
use hearthsound_server::Services;
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
use hearthsound_test_support::{ACQUISITION_FIXTURE, ALBUM_CATALOG, fixture_plugin};
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

/// The provider id prefix of the fixture's tracks.
const TRACK_PREFIX: &str = "album-catalog-track-";

/// The title of the fixture's first track.
const FIRST_TITLE: &str = "Fixture Song 1";

/// The file name the fixture's first download produces.
const FIRST_FILE: &str = "01 - Fixture Song 1.flac";

/// The path the first track is imported to, relative to the cache root.
const FIRST_REL_PATH: &str = "Fixture Artist/Fixture Album (2026)/01 - Fixture Song 1.flac";

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
const FIRST_ISRC: &str = "USHW12600001";

/// Installs the album catalog fixture and assigns it to the catalog layer.
///
/// # Parameters
///
/// - `services`: the running services.
async fn install_catalog(services: &Arc<Services>) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(ALBUM_CATALOG))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register_installed(&installed, PluginSource::Sideload)
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
/// - `scenario`: what the simulated backend does, `normal` or `no-candidates`.
/// - `running_polls`: how many polls the download reports progress before it
///   completes; the polls are two seconds apart.
async fn install_acquisition(
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
        .register_installed(&installed, PluginSource::Sideload)
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

/// Returns the catalog id of one track of the fixture's album.
///
/// # Parameters
///
/// - `number`: the track number within the album, one-based.
fn catalog_track(number: u8) -> CatalogId {
    CatalogId::new(
        Slug::new(CATALOG_SLUG).unwrap(),
        EntityKind::Track,
        format!("{TRACK_PREFIX}{number}"),
    )
    .unwrap()
}

/// Returns the catalog id of one track, as a client writes it.
///
/// # Parameters
///
/// - `number`: the track number within the album, one-based.
fn catalog_id(number: u8) -> String {
    catalog_track(number).to_string()
}

/// Resolves one track of the fixture's album to its stored row.
///
/// # Parameters
///
/// - `catalog`: the running catalog layer.
/// - `number`: the track number within the album, one-based.
async fn materialize(catalog: &CatalogService, number: u8) -> EntityId {
    catalog
        .materialize_track(&catalog_track(number))
        .await
        .unwrap()
}

/// Creates the instance's import root inside the fixture's data directory.
///
/// The directory sits beside the database, so it is the same directory tree a
/// restarted process finds again.
fn downloads_dir(fixture: &Fixture) -> PathBuf {
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
async fn place_audio(downloads: &Path) -> PathBuf {
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
async fn give_track_isrc(services: &Arc<Services>, track: EntityId, isrc: &str) {
    sqlx::query("UPDATE tracks SET isrc = ? WHERE id = ?")
        .bind(isrc)
        .bind(track.uuid().as_bytes().to_vec())
        .execute(services.database().writer())
        .await
        .unwrap();
}

/// Returns the path of a `stream` request for one id.
///
/// # Parameters
///
/// - `fixture`: the running fixture, for its api key.
/// - `id`: the track or catalog id to stream.
fn stream_path(fixture: &Fixture, id: &str) -> String {
    format!("/rest/stream?id={id}&apiKey={}", fixture.api_key)
}

/// Plays one catalog id and returns the bytes the client received.
///
/// The answer is the held pending stream: nothing is sent while the acquisition
/// runs, and once it has imported the file the response is the stored file
/// itself - `200 audio/flac` with a length and byte ranges. Nothing is written
/// before then, so the body is exactly the file.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `id`: the catalog id to stream.
///
/// # Returns
///
/// The whole body, which is the imported file.
async fn play_stream(fixture: &Fixture, id: &str) -> Vec<u8> {
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
async fn ledger_rows(
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
async fn eventual<F, Fut>(mut condition: F)
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
            &AlbumInput::new("Fixture Local Album", Origin::Local),
            &[ArtistCredit::new("Fixture Local Artist", "main")],
        )
        .await
        .unwrap();
    let mut input = TrackInput::new("Fixture Local Song", Origin::Local, Availability::Local);
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

    for kind in ["artist", "album", "song"] {
        let ids: Vec<String> = result[kind]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect();
        let first_catalog = ids
            .iter()
            .position(|id| id.starts_with("x_"))
            .unwrap_or_else(|| panic!("`{kind}` has a catalog result: {ids:?}"));
        assert!(
            first_catalog > 0,
            "`{kind}` has a local result before its catalog ones: {ids:?}"
        );
        assert!(
            ids[first_catalog..].iter().all(|id| id.starts_with("x_")),
            "no local result of `{kind}` follows a catalog one: {ids:?}"
        );
    }

    let songs: Vec<String> = result["song"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(songs[0], local.to_string(), "the local track comes first");
    assert_eq!(
        songs[1],
        catalog_id(1),
        "the catalog's own track follows it"
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

    let id = catalog_id(1);
    let track = materialize(services.catalog(), 1).await;
    give_track_isrc(&services, track, FIRST_ISRC).await;

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
    assert_eq!(probed.tags.date.as_deref(), Some("2026"));
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

/// Replaying an imported catalog id serves the stored file with a range.
#[tokio::test]
async fn a_replay_of_an_imported_track_serves_a_range() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    let id = catalog_id(1);
    let track = materialize(services.catalog(), 1).await;
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

    let id = catalog_id(1);
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

    let track = materialize(services.catalog(), 1).await;
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
            stream_path(&fixture, &catalog_id(1))
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
    let response = fixture.get(&stream_path(&fixture, &catalog_id(2))).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header(&response, RETRY_AFTER), Some("10"));
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
            let response = fixture.get(&stream_path(&fixture, &catalog_id(1))).await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(header(&response, RETRY_AFTER), Some("10"));

            let track = materialize(services.catalog(), 1).await;
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

/// Finds the catalog id of the fixture's album through `search3`.
async fn catalog_album_id(fixture: &Fixture) -> String {
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
        .find(|id| id.starts_with("x_"))
        .expect("the catalog answers the fixture album")
}

/// Stars ids through `/rest/star` and expects an ok envelope.
async fn star(fixture: &Fixture, parameter: &str, id: &str) {
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

/// Liking a catalog album acquires its tracks at priority `queue`, and the one
/// the backend can serve lands in the permanent library root.
#[tokio::test]
async fn starring_a_catalog_album_acquires_it_into_the_library() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    let album = catalog_album_id(&fixture).await;
    star(&fixture, "albumId", &album).await;

    let track = materialize(services.catalog(), 1).await;
    let rows = ledger_rows(services.orchestrator(), track).await;
    assert_eq!(rows.len(), 1, "one acquisition per album track");
    assert_eq!(rows[0].priority, hearthsound_core::Priority::Queue);
    assert_eq!(rows[0].reason, "save");
    assert_eq!(rows[0].requested_by, Some(fixture.user_id));
    for number in 2..=5 {
        let other = materialize(services.catalog(), number).await;
        assert_eq!(ledger_rows(services.orchestrator(), other).await.len(), 1);
    }
    wait_for_state(
        services.orchestrator(),
        rows[0].id,
        AcquisitionState::Completed,
    )
    .await;

    // The track is pinned by the like, so the import put it in the library
    // root rather than the cache.
    let imported = services.config().paths.library.join(FIRST_REL_PATH);
    assert!(imported.is_file(), "{} exists", imported.display());
    assert!(!services.config().paths.cache.join(FIRST_REL_PATH).exists());
    assert_eq!(
        LibraryRepository::new(services.database())
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
    star(&fixture, "id", &catalog_id(1)).await;
    let first = materialize(services.catalog(), 1).await;
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
    star(&fixture, "id", &catalog_id(2)).await;
    let second = materialize(services.catalog(), 2).await;
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
