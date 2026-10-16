//! Integration tests for the transcoding endpoints: the decision a client is
//! given for its own description, the parameters it carries, the stream the
//! parameters ask for, and the requests each endpoint refuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response as HttpResponse;
use tower::ServiceExt;

use crate::common::artwork::{Art, seed_album};
use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, assert_error, get};

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

/// Posts a client description and returns the answer's body.
///
/// # Parameters
///
/// - `fixture`: the fixture under test.
/// - `query`: the query string after the endpoint's name.
/// - `client`: the client description to post.
///
/// # Returns
///
/// The response body as text.
async fn decide(fixture: &Fixture, query: &str, client: &serde_json::Value) -> String {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/rest/getTranscodeDecision?{query}"))
        .header("content-type", "application/json")
        .body(Body::from(client.to_string()))
        .unwrap();
    let response = fixture.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A client that plays mp3 directly is told it can, and is given `direct`.
#[tokio::test]
async fn a_client_that_plays_the_file_is_told_it_can() {
    let (fixture, seeded) = fixture().await;
    let track = seeded.alpha_tracks[0];
    let body = decide(
        &fixture,
        &format!("mediaId={track}&mediaType=song&apiKey={}&f=json", fixture.api_key),
        &serde_json::json!({
            "name": "Rufin",
            "platform": "linux",
            "maxAudioBitrate": 320000,
            "directPlayProfiles": [
                { "containers": ["mp3"], "audioCodecs": ["mp3"], "protocols": ["http"] }
            ],
            "transcodingProfiles": [
                { "container": "mp3", "audioCodec": "mp3", "protocol": "http", "maxAudioChannels": 2 }
            ],
            "codecProfiles": []
        }),
    )
    .await;

    assert!(body.contains("\"canDirectPlay\":true"), "{body}");
    assert!(body.contains("\"transcodeParams\":\"direct\""), "{body}");
    assert!(body.contains("\"sourceStream\""), "{body}");
    assert!(body.contains("\"container\":\"mp3\""), "{body}");
    assert!(body.contains("\"codec\":\"mp3\""), "{body}");
    assert!(
        body.contains("\"transcodeReason\":[]"),
        "a profile that covers the file leaves no reason: {body}"
    );
}

/// A client that cannot play the file is told why and what it would be given.
#[tokio::test]
async fn a_client_that_cannot_play_the_file_is_offered_a_transcode() {
    let (fixture, seeded) = fixture().await;
    let track = seeded.alpha_tracks[0];
    let body = decide(
        &fixture,
        &format!("mediaId={track}&mediaType=song&apiKey={}&f=json", fixture.api_key),
        &serde_json::json!({
            "name": "Feishin",
            "platform": "linux",
            "directPlayProfiles": [
                { "containers": ["flac"], "audioCodecs": ["flac"] }
            ],
            "transcodingProfiles": [
                { "container": "ogg", "audioCodec": "opus", "protocol": "http", "maxAudioChannels": 1 }
            ],
            "codecProfiles": [
                {
                    "type": "Codec",
                    "name": "mp3",
                    "limitations": [
                        { "name": "audioBitrate", "comparison": "LessThanEqual", "values": ["96000"] }
                    ]
                }
            ]
        }),
    )
    .await;

    assert!(body.contains("\"canDirectPlay\":false"), "{body}");
    assert!(body.contains("\"canTranscode\":true"), "{body}");
    assert!(
        body.contains("ContainerNotSupported"),
        "the profile does not list the file's container: {body}"
    );
    // The bitrate is the lowest of the format default, the client's caps, and
    // the codec limitation.
    assert!(body.contains("\"transcodeParams\":\"opus-96-1\""), "{body}");
    assert!(body.contains("\"transcodeStream\""), "{body}");
    assert!(body.contains("\"container\":\"opus\""), "{body}");
}

/// The stream endpoint serves the bytes the parameters ask for.
#[tokio::test]
async fn the_stream_serves_what_the_parameters_ask_for() {
    // The stream is served from a file on disk, so the seed writes real audio.
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Alpha", &[Art::None]).await;
    let track = seeded.first_track();
    let app = fixture.app();

    // A direct request is the stored file.
    let direct = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/rest/getTranscodeStream?mediaId={track}&mediaType=song&transcodeParams=direct&apiKey={}",
                    fixture.api_key
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(direct.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(direct.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!bytes.is_empty(), "the stored file is served");

    // A transcode is an encoded stream, and its type is the format's.
    let transcoded = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/rest/getTranscodeStream?mediaId={track}&mediaType=song&transcodeParams=opus-96&offset=1&apiKey={}",
                    fixture.api_key
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(transcoded.status(), StatusCode::OK);
    assert_eq!(
        transcoded.headers().get("content-type").unwrap(),
        "audio/ogg",
        "the response carries the encoded stream's type"
    );
    let bytes = axum::body::to_bytes(transcoded.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!bytes.is_empty(), "the encoded stream is served");
}

/// The decision endpoint refuses what it cannot decide.
#[tokio::test]
async fn the_decision_refuses_unusable_requests() {
    let (fixture, seeded) = fixture().await;
    let track = seeded.alpha_tracks[0];

    // A GET carries no client description, so the request cannot be decided.
    let get_response = get(
        &fixture.app(),
        &format!(
            "/rest/getTranscodeDecision?mediaId={track}&mediaType=song&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&get_response, 10);

    // A body that is not the client description is a generic error.
    let bad_body = decide(
        &fixture,
        &format!(
            "mediaId={track}&mediaType=song&apiKey={}&f=json",
            fixture.api_key
        ),
        &serde_json::json!({ "name": 7 }),
    )
    .await;
    assert!(bad_body.contains("\"code\":0"), "{bad_body}");

    // A podcast is unknown here, and another media type is not defined.
    let podcast = decide(
        &fixture,
        &format!(
            "mediaId={track}&mediaType=podcast&apiKey={}&f=json",
            fixture.api_key
        ),
        &serde_json::json!({ "name": "Rufin" }),
    )
    .await;
    assert!(podcast.contains("\"code\":70"), "{podcast}");
    let unknown_type = decide(
        &fixture,
        &format!(
            "mediaId={track}&mediaType=video&apiKey={}&f=json",
            fixture.api_key
        ),
        &serde_json::json!({ "name": "Rufin" }),
    )
    .await;
    assert!(unknown_type.contains("\"code\":0"), "{unknown_type}");

    // An id that names no stored track is refused.
    let unknown = decide(
        &fixture,
        &format!(
            "mediaId=al_{track}&mediaType=song&apiKey={}&f=json",
            fixture.api_key
        ),
        &serde_json::json!({ "name": "Rufin" }),
    )
    .await;
    assert!(unknown.contains("\"code\":70"), "{unknown}");
}

/// The stream endpoint refuses unusable parameters.
#[tokio::test]
async fn the_stream_refuses_unusable_parameters() {
    let (fixture, seeded) = fixture().await;
    let track = seeded.alpha_tracks[0];
    let app = fixture.app();

    let refused = |uri: String| {
        let app = app.clone();
        async move {
            let response: HttpResponse = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            response.status()
        }
    };

    assert_eq!(
        refused(format!(
            "/rest/getTranscodeStream?mediaId={track}&transcodeParams=flac-1000&apiKey={}",
            fixture.api_key
        ))
        .await,
        StatusCode::BAD_REQUEST,
        "a format the server cannot encode"
    );
    assert_eq!(
        refused(format!(
            "/rest/getTranscodeStream?mediaId={track}&transcodeParams=opus&apiKey={}",
            fixture.api_key
        ))
        .await,
        StatusCode::BAD_REQUEST,
        "a parameter string without a bitrate"
    );
    assert_eq!(
        refused(format!(
            "/rest/getTranscodeStream?mediaId={track}&apiKey={}",
            fixture.api_key
        ))
        .await,
        StatusCode::BAD_REQUEST,
        "a request without parameters"
    );
    assert_eq!(
        refused(format!(
            "/rest/getTranscodeStream?mediaId={track}&transcodeParams=direct&offset=soon&apiKey={}",
            fixture.api_key
        ))
        .await,
        StatusCode::BAD_REQUEST,
        "an offset that is not a number"
    );
}
