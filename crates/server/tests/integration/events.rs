//! Full-stack tests of the server event stream: the credentials that open it,
//! single-use tickets, replay from `Last-Event-ID`, the refetch notice, and
//! audience filtering.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hearthsound_core::events::{Audience, PlaylistChanged, ServerEvent, UploadUpdated};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, SET_COOKIE};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use crate::common::{ACCOUNT_PASSWORD, Fixture, USERNAME};
use crate::multi_user::create_user;
use crate::native::{bearer, json as read_json};

/// How long a test waits for the next frame before failing.
const FRAME_WAIT: Duration = Duration::from_secs(5);

/// How many events overflow the replay window in the eviction tests.
const FLOOD: usize = 1_100;

/// One parsed Server-Sent Events frame.
#[derive(Debug)]
struct Frame {
    /// The `id:` line, absent on the refetch notice.
    id: Option<String>,
    /// The `event:` line, the wire event name.
    event: String,
    /// The `data:` payload, parsed as JSON.
    data: Value,
}

/// The URL of the event stream.
fn stream_url(fixture: &Fixture) -> String {
    format!("{}/api/v1/events", fixture.base_url())
}

/// The bearer header value of the fixture's API key.
fn bearer_value(fixture: &Fixture) -> String {
    format!("Bearer {}", fixture.api_key)
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
///
/// # Returns
///
/// The frames in the order they arrived.
///
/// # Panics
///
/// When a frame does not arrive within [`FRAME_WAIT`] or the stream ends.
async fn next_frames(response: &mut reqwest::Response, count: usize) -> Vec<Frame> {
    let mut buffer = String::new();
    let mut frames = Vec::new();
    while frames.len() < count {
        let chunk = tokio::time::timeout(FRAME_WAIT, response.chunk())
            .await
            .expect("no frame arrived in time")
            .unwrap()
            .expect("the stream ended early");
        buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        while let Some(end) = buffer.find("\n\n") {
            let block: String = buffer.drain(..end + 2).collect();
            if let Some(frame) = parse_frame(&block) {
                frames.push(frame);
            }
        }
    }
    frames
}

/// Reads frames until one carries the given playlist id, returning every frame
/// read up to and including it.
async fn frames_through(response: &mut reqwest::Response, playlist_id: &str) -> Vec<Frame> {
    let mut seen = Vec::new();
    loop {
        let frame = next_frames(response, 1).await.remove(0);
        let done = frame.data["data"]["playlistId"] == playlist_id;
        seen.push(frame);
        if done {
            return seen;
        }
    }
}

/// Publishes a playlist change for everyone, returning its envelope id.
fn publish_playlist(fixture: &Fixture, playlist_id: &str) -> u64 {
    fixture
        .services
        .events()
        .publish(ServerEvent::PlaylistChanged(PlaylistChanged {
            playlist_id: playlist_id.to_owned(),
            change: "updated".to_owned(),
        }))
}

/// Issues a stream ticket for the fixture's account.
async fn ticket(fixture: &Fixture) -> String {
    let response = bearer(fixture, Method::POST, "/api/v1/events/ticket", None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    read_json(response).await["ticket"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Asserts a response is a problem document with the given code.
async fn assert_problem(response: reqwest::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    let body = read_json(response).await;
    assert_eq!(body["code"], code, "{body}");
}

/// A bearer key opens the stream; a published event arrives as a frame whose
/// id, event name and payload match the envelope.
#[tokio::test]
async fn a_bearer_key_opens_the_stream() {
    let fixture = Fixture::new().await;
    let mut response = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );

    let id = publish_playlist(&fixture, "pl_bearer");
    let frames = next_frames(&mut response, 1).await;
    assert_eq!(frames[0].event, "playlist.changed");
    assert_eq!(frames[0].id, Some(id.to_string()));
    assert_eq!(frames[0].data["type"], "playlist.changed");
    assert_eq!(frames[0].data["data"]["playlistId"], "pl_bearer");

    drop(response);
    fixture.stop().await;
}

/// A session cookie opens the stream without a bearer header.
#[tokio::test]
async fn a_session_cookie_opens_the_stream() {
    let fixture = Fixture::new().await;
    let login = fixture
        .client
        .post(format!("{}/api/v1/auth/login", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(json!({ "username": USERNAME, "password": ACCOUNT_PASSWORD }).to_string())
        .send()
        .await
        .unwrap();
    assert!(login.status().is_success());
    let cookie = login
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .map(|value| {
            value
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("; ");

    let mut response = fixture
        .client
        .get(stream_url(&fixture))
        .header(COOKIE, cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    publish_playlist(&fixture, "pl_cookie");
    let frames = next_frames(&mut response, 1).await;
    assert_eq!(frames[0].data["data"]["playlistId"], "pl_cookie");

    drop(response);
    fixture.stop().await;
}

/// A ticket opens the stream once; a second use, or an unknown token, is
/// refused as unauthenticated.
#[tokio::test]
async fn a_ticket_opens_the_stream_once() {
    let fixture = Fixture::new().await;
    let token = ticket(&fixture).await;

    let mut first = fixture
        .client
        .get(format!("{}?ticket={token}", stream_url(&fixture)))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let reused = fixture
        .client
        .get(format!("{}?ticket={token}", stream_url(&fixture)))
        .send()
        .await
        .unwrap();
    assert_problem(reused, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    let unknown = fixture
        .client
        .get(format!("{}?ticket=not-a-ticket", stream_url(&fixture)))
        .send()
        .await
        .unwrap();
    assert_problem(unknown, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    publish_playlist(&fixture, "pl_ticket");
    let frames = next_frames(&mut first, 1).await;
    assert_eq!(frames[0].data["data"]["playlistId"], "pl_ticket");

    drop(first);
    fixture.stop().await;
}

/// A ticket response carries a 60-second expiry, measured from now.
#[tokio::test]
async fn a_ticket_expires_one_minute_after_it_is_issued() {
    let fixture = Fixture::new().await;
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let response = bearer(&fixture, Method::POST, "/api/v1/events/ticket", None).await;
    let body = read_json(response).await;
    let expires_at = body["expiresAt"].as_i64().unwrap();
    assert!(expires_at >= before + 60_000, "{body}");
    assert!(expires_at <= before + 61_000, "{body}");
    fixture.stop().await;
}

/// A malformed `Last-Event-ID` is refused before the ticket is redeemed, so
/// the same ticket still opens the stream afterwards.
#[tokio::test]
async fn a_malformed_last_event_id_leaves_the_ticket_usable() {
    let fixture = Fixture::new().await;
    let token = ticket(&fixture).await;

    for malformed in ["not-a-number", ""] {
        let refused = fixture
            .client
            .get(format!("{}?ticket={token}", stream_url(&fixture)))
            .header("Last-Event-ID", malformed)
            .send()
            .await
            .unwrap();
        assert_problem(refused, StatusCode::BAD_REQUEST, "invalid-request").await;
    }

    let response = fixture
        .client
        .get(format!("{}?ticket={token}", stream_url(&fixture)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    drop(response);
    fixture.stop().await;
}

/// `Last-Event-ID` replays only the events after it, in order, and the live
/// feed continues from there without repeating a replayed event.
#[tokio::test]
async fn last_event_id_replays_the_missed_events() {
    let fixture = Fixture::new().await;
    let first = publish_playlist(&fixture, "pl_one");
    publish_playlist(&fixture, "pl_two");
    publish_playlist(&fixture, "pl_three");

    let mut response = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .header("Last-Event-ID", first.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let replayed = next_frames(&mut response, 2).await;
    assert_eq!(replayed[0].data["data"]["playlistId"], "pl_two");
    assert_eq!(replayed[1].data["data"]["playlistId"], "pl_three");

    publish_playlist(&fixture, "pl_four");
    let live = next_frames(&mut response, 1).await;
    assert_eq!(live[0].data["data"]["playlistId"], "pl_four");

    drop(response);
    fixture.stop().await;
}

/// A `Last-Event-ID` older than the replay window gets the refetch notice,
/// which carries no `id` line, and the live feed continues.
#[tokio::test]
async fn an_evicted_last_event_id_sends_the_refetch_notice() {
    let fixture = Fixture::new().await;
    let oldest = publish_playlist(&fixture, "pl_oldest");
    for _ in 0..FLOOD {
        publish_playlist(&fixture, "pl_flood");
    }

    let mut response = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .header("Last-Event-ID", oldest.to_string())
        .send()
        .await
        .unwrap();
    let notice = next_frames(&mut response, 1).await.remove(0);
    assert_eq!(notice.event, "system.notice");
    assert_eq!(notice.id, None);
    assert_eq!(notice.data["data"]["code"], "events-lost");
    assert_eq!(notice.data["data"]["level"], "warn");

    publish_playlist(&fixture, "pl_after_notice");
    let live = frames_through(&mut response, "pl_after_notice").await;
    assert!(live.last().unwrap().id.is_some());

    drop(response);
    fixture.stop().await;
}

/// A `Last-Event-ID` beyond the latest id, as after a server restart, gets the
/// refetch notice; events published afterwards are delivered, not skipped.
#[tokio::test]
async fn a_last_event_id_from_a_previous_run_sends_the_refetch_notice() {
    let fixture = Fixture::new().await;
    let beyond = fixture.services.events().latest_id() + 1_000;

    let mut response = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .header("Last-Event-ID", beyond.to_string())
        .send()
        .await
        .unwrap();
    let notice = next_frames(&mut response, 1).await.remove(0);
    assert_eq!(notice.event, "system.notice");
    assert_eq!(notice.data["data"]["code"], "events-lost");

    let id = publish_playlist(&fixture, "pl_new_run");
    let live = next_frames(&mut response, 1).await;
    assert_eq!(live[0].id, Some(id.to_string()));
    assert_eq!(live[0].data["data"]["playlistId"], "pl_new_run");

    drop(response);
    fixture.stop().await;
}

/// A connection that falls behind a burst of events keeps receiving the live
/// feed: the events it can still reach arrive, and any gap is reported with
/// the refetch notice rather than ending the stream.
#[tokio::test]
async fn a_connection_that_falls_behind_recovers() {
    let fixture = Fixture::new().await;
    let mut response = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    for _ in 0..FLOOD {
        publish_playlist(&fixture, "pl_burst");
    }
    publish_playlist(&fixture, "pl_recovered");

    let frames = frames_through(&mut response, "pl_recovered").await;
    for frame in &frames {
        let playlist = frame.data["data"]["playlistId"].as_str();
        let notice = frame.event == "system.notice" && frame.data["data"]["code"] == "events-lost";
        assert!(
            notice || playlist == Some("pl_burst") || playlist == Some("pl_recovered"),
            "unexpected frame {frame:?}"
        );
    }

    drop(response);
    fixture.stop().await;
}

/// An event addressed to one user reaches that user and the administrators,
/// but not another ordinary user; a shared event reaches everyone.
#[tokio::test]
async fn audience_routing_decides_which_streams_see_an_event() {
    let fixture = Fixture::new().await;
    let (listener_id, listener_key) = create_user(&fixture, "listener", false).await;
    let events = fixture.services.events();

    let mut listener = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, format!("Bearer {listener_key}"))
        .send()
        .await
        .unwrap();
    let mut admin = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .send()
        .await
        .unwrap();

    let upload = |upload_id: &str| {
        ServerEvent::UploadUpdated(UploadUpdated {
            upload_id: upload_id.to_owned(),
            state: "receiving".to_owned(),
            bytes_received: 0,
            total_bytes: 0,
        })
    };
    events.publish_for(Audience::User(fixture.user_id), upload("up_for_admin_user"));
    events.publish_for(Audience::User(listener_id), upload("up_for_listener"));
    publish_playlist(&fixture, "pl_shared");

    let listener_frames = next_frames(&mut listener, 2).await;
    assert_eq!(
        listener_frames[0].data["data"]["uploadId"],
        "up_for_listener"
    );
    assert_eq!(listener_frames[1].data["data"]["playlistId"], "pl_shared");

    let admin_frames = next_frames(&mut admin, 3).await;
    assert_eq!(
        admin_frames[0].data["data"]["uploadId"],
        "up_for_admin_user"
    );
    assert_eq!(admin_frames[1].data["data"]["uploadId"], "up_for_listener");
    assert_eq!(admin_frames[2].data["data"]["playlistId"], "pl_shared");

    drop(listener);
    drop(admin);
    fixture.stop().await;
}

/// A scan that finished with a problem ends with a `done` frame carrying it.
///
/// The phase is what a client keys the end of the scan on, so a completed scan
/// that kept a warning says `done` and names the warning, and only a scan that
/// did not finish says `failed`.
#[tokio::test]
async fn a_scan_that_refused_to_prune_ends_with_a_done_event_carrying_the_reason() {
    let fixture = Fixture::new().await;
    let root = fixture
        .database_path()
        .parent()
        .unwrap()
        .join("music-scan-event");
    crate::native::write_album(&root, "Event Artist", "Event Album").await;

    let created = bearer(
        &fixture,
        Method::POST,
        "/api/v1/admin/folders",
        Some(json!({ "path": root, "generic": true, "watch": false })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let folder = read_json(created).await["id"].as_str().unwrap().to_owned();
    // The progress payload carries the folder's raw uuid, which the client id
    // encodes.
    let folder_uuid =
        hearthsound_api_native::views::parse_id(&folder, hearthsound_core::IdPrefix::LibraryFolder)
            .unwrap()
            .to_string();

    let mut scanned = false;
    for _ in 0..250 {
        let listed =
            read_json(bearer(&fixture, Method::GET, "/api/v1/admin/folders", None).await).await;
        let entry = listed["folders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == folder.as_str())
            .cloned()
            .unwrap();
        if entry.get("lastScanAt").is_some() {
            scanned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(scanned, "the first scan finished");

    // The music is gone; the mount point is still an empty directory.
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();

    let mut response = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let scan = bearer(
        &fixture,
        Method::POST,
        &format!("/api/v1/admin/folders/{folder}/scan"),
        None,
    )
    .await;
    assert_eq!(scan.status(), StatusCode::ACCEPTED);

    let terminal = loop {
        let frame = next_frames(&mut response, 1).await.remove(0);
        let data = &frame.data["data"];
        if frame.event == "scan.progress"
            && data["folderId"] == folder_uuid.as_str()
            && data["phase"] == "done"
        {
            break data.clone();
        }
    };
    assert_eq!(
        terminal["error"],
        "Nothing was removed: the folder root is empty, so the disk may not be mounted."
    );

    drop(response);
    fixture.stop().await;
}
