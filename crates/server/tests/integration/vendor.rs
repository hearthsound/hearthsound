//! Full-stack tests of the `hearthsound` vendor extension.
//!
//! Every test builds the real server over a temporary data directory, so a
//! request travels the whole path a client's request travels: `/rest` over real
//! HTTP, the Subsonic credential check, the vendor gate, the projections, the
//! acquisition ledger and the event stream the native API exposes. The fixture
//! plugins are the provider layers the running server serves, so the vendor
//! member, the `hs*` endpoints and the stream are exercised end to end rather
//! than against stored snapshots.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_acquire::repo::ListFilter;
use hearthsound_core::events::{EventEnvelope, ServerEvent};
use hearthsound_core::{Availability, EntityId, Priority, StorageClass};
use hearthsound_library::{LibraryRepository, ManagedMediaFile, TrackInput};
use reqwest::header::AUTHORIZATION;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::broadcast::Receiver;

use crate::common::{self, Fixture, insert_remote_track};
use crate::multi_user::create_user;
use crate::native::bearer;
use crate::on_demand::{
    downloads_dir, install_acquisition, install_catalog, place_audio, resolve_track,
};

/// How long a test waits for the next event-stream frame before failing.
const FRAME_WAIT: Duration = Duration::from_secs(10);

/// The longest gap between two download progress frames this extension allows.
///
/// The orchestrator republishes a running acquisition at least every two
/// seconds, so a gap above this proves a slow download went silent.
const CADENCE: Duration = Duration::from_millis(2_500);

/// How many polls the simulated backend reports progress for in the cadence
/// test: enough for the two-second republish to fire several times.
const RUNNING_POLLS: u32 = 100;

/// Returns a stored entity id from the client id a response carries.
///
/// # Panics
///
/// Panics when the value is not a client id that names a stored entity.
fn entity_of(raw: &str) -> EntityId {
    EntityId::parse(raw).unwrap()
}

/// Inserts a stored track with one managed library file of known quality.
///
/// The file is what makes the track's availability and `quality` member
/// real rather than hypothetical: the row carries the format, bitrate, sample
/// rate, bit depth and losslessness the vendor member reports, the availability
/// is the one a permanent file gives a track, and the bytes are written where
/// the storage class resolves them.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `rel_path`: the file's path relative to the library storage root.
///
/// # Returns
///
/// The stored track's id.
async fn stored_file_track(fixture: &Fixture, rel_path: &str) -> EntityId {
    let library = LibraryRepository::new(fixture.services.database());
    let track = library
        .insert_track(
            &TrackInput::new("Fixture File Song", Availability::Local),
            &[],
            &[],
        )
        .await
        .unwrap();
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Library,
            rel_path: rel_path.to_owned(),
            size: 4,
            mtime_ms: 1,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: Some(900),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            duration_ms: Some(4_000),
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: None,
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    let path = fixture
        .services
        .storage_roots()
        .root_for(StorageClass::Library)
        .join(rel_path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"flac").unwrap();
    track
}

/// Sends a GET to a `/rest` endpoint as the fixture's own account.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `endpoint`: the endpoint name, without the `/rest/` prefix.
/// - `query`: the parameters after the api key.
///
/// # Returns
///
/// The rendered body. A Subsonic failure is a successful HTTP answer carrying a
/// failed envelope, so only the body is returned.
async fn rest(fixture: &Fixture, endpoint: &str, query: &str) -> String {
    rest_as(fixture, &fixture.api_key, endpoint, query).await
}

/// Sends a GET to a `/rest` endpoint as the credential `key`.
async fn rest_as(fixture: &Fixture, key: &str, endpoint: &str, query: &str) -> String {
    let response = fixture
        .get(&format!("/rest/{endpoint}?apiKey={key}&{query}"))
        .await;
    assert_eq!(response.status(), StatusCode::OK, "GET /rest/{endpoint}");
    response.text().await.unwrap()
}

/// Returns the `subsonic-response` member of a rendered answer.
///
/// # Panics
///
/// Panics when the body is not the JSON envelope a `f=json` request answers.
fn subsonic(body: &str) -> Value {
    serde_json::from_str::<Value>(body).unwrap()["subsonic-response"].clone()
}

/// Asserts a rendered answer is a Subsonic failure with the given code.
fn assert_error(body: &str, code: i64) {
    let envelope = subsonic(body);
    assert_eq!(envelope["status"], "failed", "{body}");
    assert_eq!(envelope["error"]["code"], code, "{body}");
}

/// Asserts a rendered answer is a successful envelope.
fn assert_ok(body: &str) {
    let envelope = subsonic(body);
    assert_eq!(envelope["status"], "ok", "{body}");
}

/// Returns a track's `track_retention` pin flag; a track with no row is unpinned.
async fn retention_pin(fixture: &Fixture, track: EntityId) -> i64 {
    let pinned: i64 = sqlx::query_scalar("SELECT pinned FROM track_retention WHERE track_id = ?")
        .bind(track.uuid().as_bytes().to_vec())
        .fetch_optional(fixture.services.database().readers())
        .await
        .unwrap()
        .unwrap_or(0);
    pinned
}

/// Returns a track's `track_retention` pin reasons, empty when it has no row.
async fn retention_reasons(fixture: &Fixture, track: EntityId) -> String {
    let reasons: String =
        sqlx::query_scalar("SELECT pin_reasons FROM track_retention WHERE track_id = ?")
            .bind(track.uuid().as_bytes().to_vec())
            .fetch_optional(fixture.services.database().readers())
            .await
            .unwrap()
            .unwrap_or_default();
    reasons
}

/// Removes every `hearthsound` member from a rendered JSON object tree.
///
/// The decorator only ever appends a member to an object that carries an id, so
/// the bytes an opted-in request answered are the bytes a member-less build
/// answered plus those members, each preceded by the comma that separates it
/// from the member before it or by the object's opening brace. Removing the
/// member and that comma therefore recovers the member-less answer byte for
/// byte, which is what a test asserts equality against.
///
/// # Parameters
///
/// - `body`: the rendered answer of a request that carried `hearthsound=1`.
///
/// # Returns
///
/// The same answer without any vendor member.
fn without_vendor_members(body: &str) -> String {
    const MEMBER: &str = "\"hearthsound\":";
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(found) = rest.find(MEMBER) {
        let (before, after) = rest.split_at(found);
        let tail = &after[MEMBER.len()..];
        // A member value is an object, and an object member follows a comma or
        // the brace that opened its object; anything else is text inside a
        // string, which is left alone.
        let Some(value) = tail.strip_prefix('{') else {
            out.push_str(before);
            out.push_str(MEMBER);
            rest = tail;
            continue;
        };
        if !before.ends_with(',') && !before.ends_with('{') {
            out.push_str(before);
            out.push_str(MEMBER);
            rest = tail;
            continue;
        }
        out.push_str(before.strip_suffix(',').unwrap_or(before));
        rest = skip_value_object(value);
    }
    out.push_str(rest);
    out
}

/// Returns the tail of `value` after the object that starts at its first byte.
///
/// The scan counts braces outside strings and honours backslash escapes, so a
/// string member that contains a brace does not end the object early.
fn skip_value_object(value: &str) -> &str {
    let mut depth = 1usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return &value[index + character.len_utf8()..];
                }
            }
            _ => {}
        }
    }
    ""
}

/// One parsed Server-Sent Events frame.
#[derive(Debug)]
struct Frame {
    /// The `id:` line, which the envelope's sequence number carries.
    id: Option<String>,
    /// The `event:` line, the wire event name.
    event: String,
    /// The `data:` payload, parsed as JSON.
    data: Value,
}

/// One frame and the instant the reader received the chunk carrying it.
#[derive(Debug)]
struct TimedFrame {
    /// The frame.
    frame: Frame,
    /// When it arrived.
    at: Instant,
}

/// Opens the event stream with the fixture's api key.
async fn open_stream(fixture: &Fixture) -> reqwest::Response {
    let response = fixture
        .client
        .get(format!("{}/api/v1/events", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response
}

/// Parses one SSE block; `None` for a block without an event line, such as a
/// keep-alive comment.
fn parse_frame(block: &str) -> Option<Frame> {
    let mut id = None;
    let mut event = None;
    let mut data = Vec::new();
    for line in block.lines() {
        if let Some(value) = line.strip_prefix("id: ") {
            id = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("event: ") {
            event = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("data: ") {
            data.push(value);
        }
    }
    let event = event?;
    Some(Frame {
        id,
        event,
        data: serde_json::from_str(&data.join("\n")).unwrap(),
    })
}

/// Reads the next `count` frames from an open stream.
///
/// # Parameters
///
/// - `response`: the open stream.
/// - `count`: how many frames to read.
/// - `timeout`: how long to wait for one frame.
///
/// # Returns
///
/// The frames in the order they arrived, each with the instant it arrived.
///
/// # Panics
///
/// Panics when a frame does not arrive within `timeout` or the stream ends.
async fn next_frames(
    response: &mut reqwest::Response,
    count: usize,
    timeout: Duration,
) -> Vec<TimedFrame> {
    let mut buffer = String::new();
    let mut frames = Vec::new();
    while frames.len() < count {
        let chunk = tokio::time::timeout(timeout, response.chunk())
            .await
            .expect("no frame arrived in time")
            .unwrap()
            .expect("the stream ended early");
        let at = Instant::now();
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        while let Some(end) = buffer.find("\n\n") {
            let block: String = buffer.drain(..end + 2).collect();
            if let Some(frame) = parse_frame(&block) {
                frames.push(TimedFrame { frame, at });
            }
        }
    }
    frames
}

/// Returns the envelopes a subscriber received after `id`, oldest first.
///
/// # Parameters
///
/// - `receiver`: the bus subscriber.
/// - `id`: the last sequence number the caller has seen.
fn published_after(receiver: &mut Receiver<EventEnvelope>, id: u64) -> Vec<EventEnvelope> {
    let mut envelopes = Vec::new();
    while let Ok(envelope) = receiver.try_recv() {
        if envelope.id > id {
            envelopes.push(envelope);
        }
    }
    envelopes
}

/// Changes server settings through the admin API.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `settings`: the settings to write, keyed by their registry name.
async fn patch_settings(fixture: &Fixture, settings: Value) {
    let patched = bearer(
        fixture,
        Method::PATCH,
        "/api/v1/admin/settings",
        Some(json!({ "settings": settings })),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::OK);
}

/// The extension list names the vendor extension with version 1, and a request
/// that did not opt in is byte for byte the answer of a member-less build.
///
/// The second half is the stronger half: a client that does not send
/// `hearthsound=1` must not see bytes the extension added, so a `getSong` of a
/// stored track is requested twice, and the opted-in answer with its members
/// removed has to equal the plain one exactly. That holds only when the
/// decorator appends members and never rewrites or reorders anything else.
#[tokio::test]
async fn the_extension_list_advertises_hearthsound_and_the_flag_only_adds_members() {
    let fixture = Fixture::new().await;
    let track = stored_file_track(&fixture, "vendor/01 - Flag Song.flac").await;
    // Browsing answers within the account's own library by default, so the
    // track is saved into it before it is read; that is also the membership the
    // vendor member reports.
    let saved = rest(
        &fixture,
        "hsSave",
        &format!("hearthsound=1&f=json&id={track}"),
    )
    .await;
    assert_ok(&saved);

    let body = rest(&fixture, "getOpenSubsonicExtensions", "f=json").await;
    assert_ok(&body);
    assert!(
        body.contains(r#"{"name":"hearthsound","versions":[1]}"#),
        "the extension list names the vendor extension: {body}"
    );

    let plain = rest(&fixture, "getSong", &format!("f=json&id={track}")).await;
    let flagged = rest(
        &fixture,
        "getSong",
        &format!("hearthsound=1&f=json&id={track}"),
    )
    .await;
    assert_ok(&plain);
    assert_ok(&flagged);
    assert!(
        !plain.contains("\"hearthsound\":"),
        "a request that did not opt in carries no member: {plain}"
    );
    assert!(
        flagged.contains("\"hearthsound\""),
        "the opted-in answer carries the member: {flagged}"
    );
    assert_ne!(plain, flagged, "the flagged answer added members");
    assert_eq!(
        without_vendor_members(&flagged),
        plain,
        "the flagged answer is the plain answer plus the members the extension added"
    );

    fixture.stop().await;
}

/// A stored track's vendor member reports the fixture file's real state in both
/// encodings.
#[tokio::test]
async fn a_stored_track_reports_its_file_state_in_both_encodings() {
    let fixture = Fixture::new().await;
    let track = stored_file_track(&fixture, "vendor/01 - Member Song.flac").await;

    // The save is what puts the track in the account's personal library.
    let saved = rest(
        &fixture,
        "hsSave",
        &format!("hearthsound=1&f=json&id={track}"),
    )
    .await;
    assert_ok(&saved);

    let json_body = rest(
        &fixture,
        "getSong",
        &format!("hearthsound=1&f=json&id={track}"),
    )
    .await;
    assert!(
        json_body.contains(
            "\"hearthsound\":{\"availability\":\"local\",\"inLibrary\":true,\"disliked\":false,\
             \"quality\":{\"format\":\"flac\",\"bitrateKbps\":900,\
             \"sampleRate\":44100,\"bitDepth\":16,\"lossless\":true}}"
        ),
        "the member reports the file: {json_body}"
    );

    let xml_body = rest(&fixture, "getSong", &format!("hearthsound=1&id={track}")).await;
    assert!(
        xml_body.contains(
            "<hearthsound availability=\"local\" inLibrary=\"true\" disliked=\"false\">\
             <quality format=\"flac\" bitrateKbps=\"900\" \
             sampleRate=\"44100\" bitDepth=\"16\" lossless=\"true\"/></hearthsound>"
        ),
        "the XML member reports the same file: {xml_body}"
    );

    fixture.stop().await;
}

/// `hsGetServerInfo` reports the layers and the settings the server runs, and
/// follows a setting that changes.
#[tokio::test]
async fn the_server_info_follows_the_layers_and_the_settings() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", 1).await;
    services.registry().rebuild().await.unwrap();

    let body = rest(&fixture, "hsGetServerInfo", "hearthsound=1&f=json").await;
    let info = subsonic(&body)["serverInfo"].clone();
    assert_eq!(info["pendingMode"], "hold", "{body}");
    assert_eq!(info["catalogEnabled"], true, "{body}");
    assert_eq!(info["acquisitionEnabled"], true, "{body}");
    assert_eq!(
        info["layers"],
        json!({"layer": [
            {"name": "catalog", "instances": {"instance": ["album"]}},
            {"name": "acquisition", "instances": {"instance": ["acq"]}},
        ]}),
        "{body}"
    );
    assert_eq!(info["features"], json!(["prefetch", "vendor-v1"]), "{body}");

    patch_settings(
        &fixture,
        json!({"stream.pending.mode": "fail", "prefetch.enabled": false}),
    )
    .await;

    let body = rest(&fixture, "hsGetServerInfo", "hearthsound=1&f=json").await;
    let info = subsonic(&body)["serverInfo"].clone();
    assert_eq!(info["pendingMode"], "fail", "{body}");
    assert_eq!(info["catalogEnabled"], true, "{body}");
    assert_eq!(info["acquisitionEnabled"], true, "{body}");
    assert_eq!(
        info["features"],
        json!(["vendor-v1"]),
        "prefetching off drops the feature: {body}"
    );

    fixture.stop().await;
}

/// The availability, acquire and listing endpoints answer the contract, and the
/// vendor gate and the acquire permission are refused before any work.
#[tokio::test]
async fn availability_and_acquisitions_follow_the_vendor_contract() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    // The fixture from `common` keeps its search running for far longer than the
    // test, so the acquisitions it starts stay in the ledger while the test
    // reads them.
    common::install_acquisition(&services, &downloads).await;
    services.registry().rebuild().await.unwrap();

    let library = LibraryRepository::new(services.database());
    let remote = insert_remote_track(&library).await;
    let local = stored_file_track(&fixture, "vendor/01 - Known Song.flac").await;

    // One entry per requested id, in request order, each with its own state.
    let body = rest(
        &fixture,
        "hsGetAvailability",
        &format!("hearthsound=1&f=json&id={remote}&id={local}"),
    )
    .await;
    let entries = subsonic(&body)["availabilities"]["availability"].clone();
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 2, "{body}");
    assert_eq!(entries[0]["id"].as_str(), Some(remote.to_string().as_str()));
    assert_eq!(entries[0]["state"], "remote", "{body}");
    assert_eq!(entries[0]["inLibrary"], false, "{body}");
    assert_eq!(entries[1]["id"].as_str(), Some(local.to_string().as_str()));
    assert_eq!(entries[1]["state"], "local", "{body}");

    // The same read without the flag is answered as an unknown endpoint.
    let body = rest(
        &fixture,
        "hsGetAvailability",
        &format!("f=json&id={remote}"),
    )
    .await;
    assert_error(&body, 70);

    // An acquire request answers the row admission wrote.
    let body = rest(
        &fixture,
        "hsAcquire",
        &format!("hearthsound=1&f=json&id={remote}"),
    )
    .await;
    assert_ok(&body);
    let rows = subsonic(&body)["acquisitions"]["acquisition"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(
        rows[0]["targetId"].as_str(),
        Some(remote.to_string().as_str())
    );
    assert_eq!(rows[0]["priority"], "queue", "{body}");
    let acquisition = rows[0]["id"].as_str().unwrap().to_owned();
    assert!(acquisition.starts_with("ac_"), "{body}");

    // The listing shows the same row for its requester.
    let body = rest(
        &fixture,
        "hsGetAcquisitions",
        "hearthsound=1&f=json&state=active",
    )
    .await;
    let rows = subsonic(&body)["acquisitions"]["acquisition"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["id"].as_str(), Some(acquisition.as_str()));
    assert_eq!(
        rows[0]["targetId"].as_str(),
        Some(remote.to_string().as_str())
    );

    // Without the flag the listing is an unknown endpoint too.
    let body = rest(&fixture, "hsGetAcquisitions", "f=json").await;
    assert_error(&body, 70);

    // A principal without the acquire permission is refused before anything is
    // requested, so the ledger gains no row for the track it named.
    let (_, limited) = create_user(&fixture, "limited", false).await;
    let body = rest_as(
        &fixture,
        &limited,
        "hsAcquire",
        &format!("hearthsound=1&f=json&id={local}"),
    )
    .await;
    assert_error(&body, 50);
    let body = rest(
        &fixture,
        "hsGetAcquisitions",
        "hearthsound=1&f=json&state=all",
    )
    .await;
    let rows = subsonic(&body)["acquisitions"]["acquisition"].clone();
    assert_eq!(
        rows.as_array().unwrap().len(),
        1,
        "the refused request started nothing: {body}"
    );

    fixture.stop().await;
}

/// A download reports its progress on the event stream at least every two
/// seconds, and its completion with the availability the fixture's file gave.
#[tokio::test]
async fn the_stream_reports_download_progress_on_a_steady_cadence() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    install_catalog(&services).await;
    install_acquisition(&services, &downloads, "normal", RUNNING_POLLS).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    // The stream opens before anything is requested, so every frame it carries
    // belongs to the acquisition the test starts.
    let acquired = resolve_track(services.catalog(), 1).await;
    let mut stream = open_stream(&fixture).await;
    let body = rest(
        &fixture,
        "hsAcquire",
        &format!("hearthsound=1&f=json&id={acquired}"),
    )
    .await;
    assert_ok(&body);
    let rows = subsonic(&body)["acquisitions"]["acquisition"].clone();
    let target = rows.as_array().unwrap()[0]["targetId"]
        .as_str()
        .unwrap()
        .to_owned();
    let track = entity_of(&target);

    let mut progress: Vec<(String, Instant)> = Vec::new();
    let mut imported = None;
    for _ in 0..512 {
        let frame = next_frames(&mut stream, 1, FRAME_WAIT).await.remove(0);
        let payload = frame.frame.data["data"].clone();
        match frame.frame.event.as_str() {
            "acquisition.updated" if payload["targetId"].as_str() == Some(target.as_str()) => {
                progress.push((
                    payload["state"].as_str().unwrap_or_default().to_owned(),
                    frame.at,
                ));
            }
            "track.availability"
                if payload["trackId"].as_str() == Some(target.as_str())
                    && payload["availability"].as_str() != Some("acquiring") =>
            {
                imported = Some(payload["availability"].as_str().unwrap().to_owned());
                break;
            }
            _ => {}
        }
    }

    assert!(
        progress.len() >= 2,
        "the download published {} progress frames, so no cadence is visible",
        progress.len()
    );
    for pair in progress.windows(2) {
        let gap = pair[1].1.duration_since(pair[0].1);
        assert!(
            gap <= CADENCE,
            "two progress frames were {gap:?} apart, between `{}` and `{}`",
            pair[0].0,
            pair[1].0
        );
    }
    assert!(
        progress.iter().any(|(state, _)| state == "downloading"),
        "the frames report the download: {progress:?}"
    );
    assert_eq!(
        progress.last().unwrap().0,
        "completed",
        "the last progress frame is the completed acquisition: {progress:?}"
    );

    let imported = imported.expect("the stream reported the imported file");
    let stored = LibraryRepository::new(services.database())
        .get_track(track)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        imported,
        stored.availability.as_str(),
        "the event reports the availability the fixture's file left behind"
    );
    assert!(
        matches!(
            stored.availability,
            Availability::Local | Availability::Cached
        ),
        "the imported track is served from a file, not the catalog: {:?}",
        stored.availability
    );

    drop(stream);
    fixture.stop().await;
}

/// A declared queue prefetches the three items after the playing one, at the
/// priorities the playing order gives them.
#[tokio::test]
async fn declaring_a_queue_prefetches_the_next_three_items_in_play_order() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    common::install_acquisition(&services, &downloads).await;
    services.registry().rebuild().await.unwrap();

    let library = LibraryRepository::new(services.database());
    let mut tracks = Vec::new();
    for _ in 0..4 {
        tracks.push(insert_remote_track(&library).await);
    }

    let mut events = services.events().subscribe();
    let body = rest(
        &fixture,
        "hsSetQueue",
        &format!(
            "hearthsound=1&f=json&client=symfonium&current={}&next={}&next={}&next={}",
            tracks[0], tracks[1], tracks[2], tracks[3]
        ),
    )
    .await;
    assert_ok(&body);
    assert!(
        !body.contains("\"error\""),
        "the declaration is answered with an empty envelope: {body}"
    );

    // The declaration records the queue and starts the look-ahead before it
    // answers, so the ledger already holds one acquisition per upcoming item.
    // Each carries the priority the play order gives it: the item the client
    // plays next is acquired first and at `next`, and the two after it follow at
    // `queue`.
    let orchestrator = services.orchestrator();
    let listed = orchestrator.list(&ListFilter::default()).await.unwrap();
    let mut rows = Vec::new();
    for summary in listed {
        let row = orchestrator
            .repository()
            .get(summary.id)
            .await
            .unwrap()
            .unwrap();
        if row.reason == "prefetch" {
            rows.push(row);
        }
    }
    let expected = [
        (tracks[1], Priority::Next),
        (tracks[2], Priority::Queue),
        (tracks[3], Priority::Queue),
    ];
    assert_eq!(rows.len(), expected.len(), "{rows:?}");
    for (track, priority) in expected {
        let row = rows
            .iter()
            .find(|row| row.track_id == Some(track))
            .unwrap_or_else(|| panic!("{track} was prefetched: {rows:?}"));
        assert_eq!(row.priority, priority, "{row:?}");
        assert_eq!(row.requested_by, Some(fixture.user_id), "{row:?}");
    }

    // The same three rows are what the vendor listing reports.
    let body = rest(
        &fixture,
        "hsGetAcquisitions",
        "hearthsound=1&f=json&state=active",
    )
    .await;
    let rows = subsonic(&body)["acquisitions"]["acquisition"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), expected.len(), "{body}");
    for (track, priority) in expected {
        let row = rows
            .iter()
            .find(|row| row["targetId"].as_str() == Some(track.to_string().as_str()))
            .unwrap_or_else(|| panic!("{track} has an acquisition: {body}"));
        assert_eq!(row["priority"], priority.as_str(), "{body}");
    }

    // Every started look-ahead is announced, and the scheduler starts the items
    // one after another in the order the queue declares them, so the events'
    // order is the play order: the frame the client plays next comes first.
    let mut prefetches = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        if let ServerEvent::QueuePrefetch(prefetch) = envelope.event {
            prefetches.push(prefetch);
        }
    }
    assert_eq!(prefetches.len(), expected.len(), "{prefetches:?}");
    for (prefetch, (track, priority)) in prefetches.iter().zip(expected) {
        assert_eq!(prefetch.track_id, track.to_string(), "{prefetch:?}");
        assert_eq!(prefetch.priority, priority, "{prefetch:?}");
        assert_eq!(prefetch.client, "symfonium", "{prefetch:?}");
        assert_eq!(prefetch.state, "started", "{prefetch:?}");
    }

    fixture.stop().await;
}

/// Reconnecting with the last seen event id replays exactly what was published
/// while no stream was open, and repeats nothing the first stream saw.
///
/// The frames the bus received after the first stream's last id are the ground
/// truth for "published while nobody was connected": the resumed stream has to
/// deliver them in the order the bus published them, with the same ids and the
/// same payloads, and must not deliver the frame the first stream already saw
/// before them.
#[tokio::test]
async fn reconnecting_with_the_last_event_id_replays_the_missed_frames() {
    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = downloads_dir(&fixture);
    common::install_acquisition(&services, &downloads).await;
    services.registry().rebuild().await.unwrap();

    let library = LibraryRepository::new(services.database());
    let mut tracks = Vec::new();
    for _ in 0..3 {
        tracks.push(insert_remote_track(&library).await);
    }

    // Recording the bus before the first request means the test can say what was
    // published while the connection was closed, instead of guessing its size.
    let mut recorder = services.events().subscribe();

    let mut stream = open_stream(&fixture).await;
    let body = rest(
        &fixture,
        "hsAcquire",
        &format!("hearthsound=1&f=json&id={}", tracks[0]),
    )
    .await;
    assert_ok(&body);
    // An admitted acquisition publishes its progress and the track's new
    // availability, so two frames belong to the first request.
    let seen = next_frames(&mut stream, 2, FRAME_WAIT).await;
    assert_eq!(seen[0].frame.event, "acquisition.updated");
    assert_eq!(
        seen[0].frame.data["data"]["targetId"].as_str(),
        Some(tracks[0].to_string().as_str())
    );
    assert_eq!(seen[1].frame.event, "track.availability");
    assert_eq!(
        seen[1].frame.data["data"]["trackId"].as_str(),
        Some(tracks[0].to_string().as_str())
    );
    let resume_after: u64 = seen[1].frame.id.as_deref().unwrap().parse().unwrap();
    drop(stream);

    // Two more acquisitions run with no stream connected.
    for track in &tracks[1..] {
        let body = rest(
            &fixture,
            "hsAcquire",
            &format!("hearthsound=1&f=json&id={track}"),
        )
        .await;
        assert_ok(&body);
    }
    let missed = published_after(&mut recorder, resume_after);
    assert!(
        missed.len() >= 4,
        "two admissions publish at least four frames, got {missed:?}"
    );

    let mut resumed = fixture
        .client
        .get(format!("{}/api/v1/events", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .header("Last-Event-ID", resume_after.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), StatusCode::OK);

    let replayed = next_frames(&mut resumed, missed.len(), FRAME_WAIT).await;
    for (index, (frame, envelope)) in replayed.iter().zip(&missed).enumerate() {
        assert_eq!(
            frame.frame.id.as_deref(),
            Some(envelope.id.to_string().as_str()),
            "frame {index} resumes where the first stream stopped"
        );
        assert_eq!(
            frame.frame.event,
            envelope.event.type_str(),
            "frame {index} carries the event the bus published"
        );
        assert_eq!(
            frame.frame.data,
            serde_json::to_value(envelope).unwrap(),
            "frame {index} carries the envelope the bus published"
        );
    }
    assert!(
        replayed
            .iter()
            .all(|frame| frame.frame.id.as_deref().unwrap().parse::<u64>().unwrap() > resume_after),
        "the replay repeats nothing the first stream saw"
    );

    drop(resumed);
    fixture.stop().await;
}

/// A temporary playlist pins none of its tracks, and converting it to permanent
/// pins them again.
#[tokio::test]
async fn converting_a_playlist_to_permanent_pins_its_tracks() {
    let fixture = Fixture::new().await;
    let library = LibraryRepository::new(fixture.services.database());
    let tracks = [
        insert_remote_track(&library).await,
        insert_remote_track(&library).await,
    ];

    // A playlist a client creates is permanent, so it pins what it holds; the
    // conversion to temporary is what releases the tracks.
    let body = rest(
        &fixture,
        "createPlaylist",
        &format!(
            "f=json&name=Roadtrip&songId={}&songId={}",
            tracks[0], tracks[1]
        ),
    )
    .await;
    assert_ok(&body);
    let playlist = subsonic(&body)["playlist"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for track in tracks {
        assert_eq!(
            retention_pin(&fixture, track).await,
            1,
            "the new playlist pins"
        );
        assert!(
            retention_reasons(&fixture, track)
                .await
                .contains("playlist"),
            "the playlist is the reason the track is pinned"
        );
    }

    let body = rest(
        &fixture,
        "hsSetPlaylistLifetime",
        &format!("hearthsound=1&f=json&playlistId={playlist}&temporary=true"),
    )
    .await;
    assert_ok(&body);
    for track in tracks {
        assert_eq!(
            retention_pin(&fixture, track).await,
            0,
            "a temporary playlist pins none of its tracks"
        );
        assert!(
            !retention_reasons(&fixture, track)
                .await
                .contains("playlist"),
            "the playlist is no longer a reason to keep the track"
        );
    }

    let body = rest(
        &fixture,
        "hsSetPlaylistLifetime",
        &format!("hearthsound=1&f=json&playlistId={playlist}&temporary=false"),
    )
    .await;
    assert_ok(&body);
    for track in tracks {
        assert_eq!(
            retention_pin(&fixture, track).await,
            1,
            "converting the playlist to permanent pins its tracks"
        );
        assert!(
            retention_reasons(&fixture, track)
                .await
                .contains("playlist"),
            "the playlist holds the track again"
        );
    }

    fixture.stop().await;
}
