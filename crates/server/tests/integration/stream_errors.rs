//! Full-stack tests of a stream the server cannot produce.
//!
//! A transcode's status is committed before its body is read by the client, so
//! the only moment an encoder failure can still be answered as a failure is
//! before the first chunk exists. These tests take a stored track whose file is
//! gone and ask for it in a format that has to be encoded, which is the case that
//! used to answer `200` and then abort the body.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;

use crate::client_checklist::seed_library;
use crate::common::Fixture;

/// A stored track whose file was removed answers an error status, not a `200`
/// whose body aborts halfway.
#[tokio::test]
async fn a_transcode_of_a_missing_file_answers_an_error_not_an_aborted_200() {
    let fixture = Fixture::new().await;
    let library = seed_library(&fixture).await;
    let track = library.tracks[0];

    // The scan stored the rows and the paths; the files are gone, so ffmpeg has
    // nothing to read and fails before its first chunk.
    std::fs::remove_dir_all(&library.folder).unwrap();

    let response = fixture
        .get(&format!(
            "/rest/stream?id={track}&maxBitRate=128&format=mp3&apiKey={}",
            fixture.api_key
        ))
        .await;
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .map(|value| value.to_str().unwrap_or_default().to_owned());
    let body = response.bytes().await.unwrap();

    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a failed encode is a server error, not a 200"
    );
    assert!(
        content_type.as_deref() != Some("audio/mpeg"),
        "an error is not announced as the encoded stream: {content_type:?}"
    );
    assert!(
        body.is_empty(),
        "the failure is the status line, not the start of a body that aborts: {} bytes",
        body.len()
    );
}
