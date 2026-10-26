//! Full-stack tests of the server event stream: the credentials that open it,
//! single-use tickets, replay from `Last-Event-ID`, the refetch notice, and
//! audience filtering.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hearthsound_core::events::{Audience, PlaylistChanged, ServerEvent, UploadUpdated};
use hearthsound_core::{Availability, EntityId};
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin, TrackInput};
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
    let root = fixture.library_dir("music-scan-event");
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

/// Reads the stream until it ends, or returns `false` when it is still open
/// after the wait.
async fn ends_within(response: &mut reqwest::Response, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, response.chunk()).await {
            Ok(Ok(Some(_))) => continue,
            Ok(Ok(None) | Err(_)) => return true,
            Err(_elapsed) => return false,
        }
    }
}

/// Issues a ticket with a credential other than the fixture's own.
async fn ticket_with(
    fixture: &Fixture,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = fixture
        .client
        .request(method, format!("{}{path}", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

/// A ninth outstanding ticket is refused with a rate limit that says when the
/// oldest one expires, and redeeming one frees a slot.
#[tokio::test]
async fn a_client_cannot_hold_more_than_eight_tickets() {
    let fixture = Fixture::new().await;

    let mut issued = Vec::new();
    for _ in 0..8 {
        let response = bearer(&fixture, Method::POST, "/api/v1/events/ticket", None).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        issued.push(
            read_json(response).await["ticket"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let limited = bearer(&fixture, Method::POST, "/api/v1/events/ticket", None).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry = limited.headers()[reqwest::header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!((1..=60).contains(&retry), "Retry-After is {retry}");
    assert_eq!(read_json(limited).await["code"], "rate-limited");

    // Redeeming one frees the slot, so a reconnecting client is not stuck.
    let stream = fixture
        .client
        .get(format!("{}?ticket={}", stream_url(&fixture), issued[0]))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let again = bearer(&fixture, Method::POST, "/api/v1/events/ticket", None).await;
    assert_eq!(again.status(), StatusCode::CREATED);

    drop(stream);
    fixture.stop().await;
}

/// A ticket issued moments before its key is revoked cannot open a stream, and
/// stays spent.
#[tokio::test]
async fn a_ticket_for_a_revoked_key_cannot_open_a_stream() {
    let fixture = Fixture::new().await;
    let created = bearer(
        &fixture,
        Method::POST,
        "/api/v1/me/api-keys",
        Some(json!({ "name": "streamer" })),
    )
    .await;
    let created = read_json(created).await;
    let second = created["secret"].as_str().unwrap().to_owned();
    let second_id = created["id"].as_str().unwrap().to_owned();

    let issued = ticket_with(
        &fixture,
        Method::POST,
        "/api/v1/events/ticket",
        &[("authorization", &format!("Bearer {second}"))],
    )
    .await;
    assert_eq!(issued.status(), StatusCode::CREATED);
    let ticket = read_json(issued).await["ticket"]
        .as_str()
        .unwrap()
        .to_owned();

    let revoked = bearer(
        &fixture,
        Method::DELETE,
        &format!("/api/v1/me/api-keys/{second_id}"),
        None,
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);

    let refused = fixture
        .client
        .get(format!("{}?ticket={ticket}", stream_url(&fixture)))
        .send()
        .await
        .unwrap();
    assert_problem(refused, StatusCode::UNAUTHORIZED, "unauthenticated").await;
    let spent = fixture
        .client
        .get(format!("{}?ticket={ticket}", stream_url(&fixture)))
        .send()
        .await
        .unwrap();
    assert_problem(spent, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    fixture.stop().await;
}

/// A ticket issued before a logout cannot open a stream afterwards.
#[tokio::test]
async fn a_ticket_for_a_logged_out_session_cannot_open_a_stream() {
    let fixture = Fixture::new().await;
    let signed_in = fixture
        .client
        .post(format!("{}/api/v1/auth/login", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(json!({ "username": USERNAME, "password": ACCOUNT_PASSWORD }).to_string())
        .send()
        .await
        .unwrap();
    let cookie = signed_in.headers()[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = read_json(signed_in).await["csrfToken"]
        .as_str()
        .unwrap()
        .to_owned();

    let issued = ticket_with(
        &fixture,
        Method::POST,
        "/api/v1/events/ticket",
        &[("cookie", &cookie), ("x-hearthsound-csrf", &csrf)],
    )
    .await;
    assert_eq!(issued.status(), StatusCode::CREATED);
    let ticket = read_json(issued).await["ticket"]
        .as_str()
        .unwrap()
        .to_owned();

    let out = ticket_with(
        &fixture,
        Method::POST,
        "/api/v1/auth/logout",
        &[("cookie", &cookie), ("x-hearthsound-csrf", &csrf)],
    )
    .await;
    assert_eq!(out.status(), StatusCode::NO_CONTENT);

    let refused = fixture
        .client
        .get(format!("{}?ticket={ticket}", stream_url(&fixture)))
        .send()
        .await
        .unwrap();
    assert_problem(refused, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    fixture.stop().await;
}

/// Revoking the key behind an open stream ends it within the re-check window.
#[tokio::test]
async fn a_revoked_api_key_ends_the_stream() {
    let fixture = Fixture::new().await;
    // A second key, which stays valid and revokes the first.
    let created = bearer(
        &fixture,
        Method::POST,
        "/api/v1/me/api-keys",
        Some(json!({ "name": "revoker" })),
    )
    .await;
    let second = read_json(created).await["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let listed = bearer(&fixture, Method::GET, "/api/v1/me/api-keys", None).await;
    let listing = read_json(listed).await;
    let fixture_key_id = listing
        .as_array()
        .unwrap()
        .iter()
        .find(|key| {
            key["prefix"]
                .as_str()
                .is_some_and(|prefix| fixture.api_key.starts_with(prefix))
        })
        .map(|key| key["id"].as_str().unwrap().to_owned());
    let fixture_key_id = match fixture_key_id {
        Some(id) => id,
        None => {
            // The prefix match depends on the listing order; the fixture key is
            // the one the fixture named "test", so fall back to that.
            listing
                .as_array()
                .unwrap()
                .iter()
                .find(|key| key["name"] == "test")
                .map(|key| key["id"].as_str().unwrap().to_owned())
                .unwrap()
        }
    };

    let mut stream = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);

    let revoked = ticket_with(
        &fixture,
        Method::DELETE,
        &format!("/api/v1/me/api-keys/{fixture_key_id}"),
        &[("authorization", &format!("Bearer {second}"))],
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);

    assert!(
        ends_within(&mut stream, Duration::from_secs(8)).await,
        "the stream must end once its key is revoked"
    );
    // And it cannot reconnect.
    let refused = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .send()
        .await
        .unwrap();
    assert_problem(refused, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    drop(stream);
    fixture.stop().await;
}

/// Signing out ends an open stream within the re-check window.
#[tokio::test]
async fn a_logged_out_session_ends_the_stream() {
    let fixture = Fixture::new().await;
    let signed_in = fixture
        .client
        .post(format!("{}/api/v1/auth/login", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(json!({ "username": USERNAME, "password": ACCOUNT_PASSWORD }).to_string())
        .send()
        .await
        .unwrap();
    let cookie = signed_in.headers()[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = read_json(signed_in).await["csrfToken"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut stream = fixture
        .client
        .get(stream_url(&fixture))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);

    let out = ticket_with(
        &fixture,
        Method::POST,
        "/api/v1/auth/logout",
        &[("cookie", &cookie), ("x-hearthsound-csrf", &csrf)],
    )
    .await;
    assert_eq!(out.status(), StatusCode::NO_CONTENT);

    assert!(
        ends_within(&mut stream, Duration::from_secs(8)).await,
        "the stream must end once its session is gone"
    );

    drop(stream);
    fixture.stop().await;
}

/// Reads the stream for a while and reports whether a frame arrived.
async fn no_frame_within(response: &mut reqwest::Response, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    let mut buffer = String::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        match tokio::time::timeout(left, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buffer.push_str(std::str::from_utf8(&chunk).unwrap());
                while let Some(end) = buffer.find("\n\n") {
                    let block: String = buffer.drain(..end + 2).collect();
                    if parse_frame(&block).is_some() {
                        return false;
                    }
                }
            }
            Ok(Ok(None) | Err(_)) => return true,
            Err(_elapsed) => return true,
        }
    }
}

/// A demoted administrator stops seeing administrator events within the
/// re-check window, while a fresh administrator stream still receives them.
#[tokio::test]
async fn a_demoted_administrator_loses_administrator_events() {
    use hearthsound_auth::NewUser;

    let fixture = Fixture::new().await;
    // A second administrator, created through the auth layer so its key is
    // known, and demoted through the API afterwards.
    let second = hearthsound_auth::Users::new(fixture.services.database())
        .create(
            &fixture.services.settings().current(),
            &NewUser::new("ada", "Ada", "correct horse", true),
        )
        .await
        .unwrap();
    let key = hearthsound_auth::ApiKeys::new(fixture.services.database())
        .generate(second.id, "streamer")
        .await
        .unwrap()
        .plaintext;

    let mut stream = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, format!("Bearer {key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);

    // The PATCH needs the account's client id; the listing has it.
    let listing = bearer(&fixture, Method::GET, "/api/v1/admin/users", None).await;
    let listing = read_json(listing).await;
    let target = listing["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["username"] == "ada")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let demoted = bearer(
        &fixture,
        Method::PATCH,
        &format!("/api/v1/admin/users/{target}"),
        Some(json!({ "isAdmin": false })),
    )
    .await;
    assert_eq!(demoted.status(), StatusCode::OK);

    // Give the running stream time to re-check, then publish an event only
    // administrators see.
    tokio::time::sleep(Duration::from_secs(6)).await;
    fixture.services.events().publish_for(
        Audience::Admins,
        ServerEvent::UploadUpdated(UploadUpdated {
            upload_id: "up_admins_only".to_owned(),
            state: "receiving".to_owned(),
            bytes_received: 0,
            total_bytes: 0,
        }),
    );

    assert!(
        no_frame_within(&mut stream, Duration::from_secs(3)).await,
        "a demoted administrator must not receive administrator events"
    );

    // A fresh administrator stream does receive it.
    let mut admin = fixture
        .client
        .get(stream_url(&fixture))
        .header(AUTHORIZATION, bearer_value(&fixture))
        .send()
        .await
        .unwrap();
    fixture.services.events().publish_for(
        Audience::Admins,
        ServerEvent::UploadUpdated(UploadUpdated {
            upload_id: "up_admins_again".to_owned(),
            state: "receiving".to_owned(),
            bytes_received: 0,
            total_bytes: 0,
        }),
    );
    let frames = next_frames(&mut admin, 1).await;
    assert_eq!(frames[0].data["data"]["uploadId"], "up_admins_again");

    drop(stream);
    drop(admin);
    fixture.stop().await;
}

/// The enrichment job publishes its completion to the listener whose page asked.
#[tokio::test]
async fn a_finished_enrichment_is_announced_to_its_listener_once() {
    use hearthsound_catalog::{ENRICH_JOB_KIND, EnrichJobPayload};
    use hearthsound_core::providers::CatalogQuery;
    use hearthsound_core::{ClientId, EntityKind};
    use hearthsound_jobs::{NewJob, Shutdown};

    let fixture = Fixture::new().await;
    let services = std::sync::Arc::clone(&fixture.services);
    crate::on_demand::install_catalog(&services).await;
    services.registry().rebuild().await.unwrap();

    // A catalog artist exists, so the run has something to fetch.
    let artist = services
        .catalog()
        .search(&CatalogQuery {
            text: "Fixture".to_owned(),
            kinds: vec![EntityKind::Artist],
            limit: 10,
            offset: 0,
        })
        .await
        .unwrap()
        .artists
        .into_iter()
        .next()
        .expect("the fixture answers an artist");
    let id = ClientId::Catalog(artist.id.clone());
    let listener = fixture.user_id;

    let mut events = services.events().subscribe();
    let shutdown = Shutdown::new();
    let runner = std::sync::Arc::clone(services.runner());
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { runner.run(shutdown).await }
    });

    assert!(services.catalog().begin_enrichment(&id, listener).await);
    services
        .queue()
        .enqueue(
            NewJob::new(ENRICH_JOB_KIND)
                .priority(hearthsound_core::Priority::Queue)
                .payload(&EnrichJobPayload {
                    entity_id: id.to_string(),
                    kind: EntityKind::Artist,
                    user_id: listener,
                })
                .unwrap(),
        )
        .await
        .unwrap();

    let envelope = tokio::time::timeout(FRAME_WAIT, async {
        loop {
            let envelope = events.recv().await.expect("the bus stays open");
            if envelope.event.type_str() == "catalog.enriched" {
                return envelope;
            }
        }
    })
    .await
    .expect("the completion is published");

    match &envelope.event {
        ServerEvent::CatalogEnriched(payload) => {
            assert_eq!(payload.entity_id, id.to_string());
            assert_eq!(payload.kind, EntityKind::Artist);
        }
        other => panic!(
            "expected a catalog.enriched event, got {}",
            other.type_str()
        ),
    }
    assert_eq!(envelope.audience, Audience::User(listener));
    assert!(envelope.audience.is_visible_to(listener, false));
    assert!(
        !envelope
            .audience
            .is_visible_to(uuid::Uuid::from_u128(99), false)
    );

    // The extras are cached now: a cached read answers without a provider call.
    assert!(
        services
            .catalog()
            .cached_artist(&artist.id)
            .await
            .unwrap()
            .is_some(),
        "the run cached the artist"
    );
    assert!(
        services.catalog().begin_enrichment(&id, listener).await,
        "a finished run releases its claim"
    );

    shutdown.trigger();
    handle.await.unwrap();
}

/// Inserts one stored track and returns its client id.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn seed_track(fixture: &Fixture) -> EntityId {
    let library = LibraryRepository::new(fixture.services.database());
    let album = library
        .upsert_album(
            &AlbumInput::new("Event Album", Origin::Local),
            &[ArtistCredit::new("Event Artist", "main")],
        )
        .await
        .unwrap();
    let mut input = TrackInput::new("Event Song", Origin::Local, Availability::Local);
    input.album_id = Some(album);
    input.display_artist = Some("Event Artist".to_owned());
    input.duration_ms = Some(180_000);
    library
        .insert_track(&input, &[ArtistCredit::new("Event Artist", "main")], &[])
        .await
        .unwrap()
}

/// Opens a stream with one credential.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `key`: the API key the stream is opened with.
async fn open_stream(fixture: &Fixture, key: &str) -> reqwest::Response {
    let response = fixture
        .client
        .get(stream_url(fixture))
        .header(AUTHORIZATION, format!("Bearer {key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response
}

/// Liking a song over the Subsonic surface publishes `library.changed` to that
/// account and to administrators, not to another ordinary user.
#[tokio::test]
async fn a_like_reaches_its_own_account_and_admins_only() {
    let fixture = Fixture::new().await;
    let track = seed_track(&fixture).await.to_string();
    let (owner_id, owner_key) = create_user(&fixture, "owner", false).await;
    let (_other_id, other_key) = create_user(&fixture, "bystander", false).await;

    let mut owner = open_stream(&fixture, &owner_key).await;
    let mut admin = open_stream(&fixture, &fixture.api_key).await;
    let mut other = open_stream(&fixture, &other_key).await;

    let starred = fixture
        .get(&format!("/rest/star?id={track}&apiKey={owner_key}&f=json"))
        .await;
    assert_eq!(starred.status(), StatusCode::OK);

    let owner_frame = next_frames(&mut owner, 1).await.remove(0);
    assert_eq!(owner_frame.event, "library.changed");
    assert_eq!(owner_frame.data["data"]["entityKind"], "track");
    assert_eq!(owner_frame.data["data"]["entityId"], track.as_str());
    assert_eq!(owner_frame.data["data"]["change"], "added");

    let admin_frame = next_frames(&mut admin, 1).await.remove(0);
    assert_eq!(admin_frame.event, "library.changed");
    assert_eq!(admin_frame.data["data"]["entityId"], track.as_str());

    // The bystander's stream stays silent: the next frame it sees is the marker
    // published after the like.
    publish_playlist(&fixture, "pl_after_like");
    let other_frames = next_frames(&mut other, 1).await;
    assert_eq!(other_frames[0].event, "playlist.changed");
    assert_eq!(
        other_frames[0].data["data"]["playlistId"], "pl_after_like",
        "the bystander received an event that was not addressed to it"
    );

    let _ = owner_id;
    drop(owner);
    drop(admin);
    drop(other);
    fixture.stop().await;
}
