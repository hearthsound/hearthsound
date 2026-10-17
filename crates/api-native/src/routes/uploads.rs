//! `/api/v1/uploads`: audio files the caller sends into the caller's library.
//!
//! The request is `multipart/form-data` with one `file` part per file. Each
//! part is streamed into `<storage>/uploads-staging/<upload-id>/` while it is
//! read and then handed to the library's ingest, which moves it into
//! `<storage>/library` and indexes it as a track of the uploader's library.
//! The staging directory of every upload is removed once the upload has an
//! outcome, whatever that outcome was. Progress is published as
//! `upload.updated` events.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::multipart::{Field, Multipart};
use axum::extract::{FromRequest, Request, State};
use axum::http::header::CONTENT_LENGTH;
use axum::http::{HeaderMap, StatusCode};
use hearthsound_config::keys;
use hearthsound_core::events::{ServerEvent, UploadUpdated};
use hearthsound_core::{IdPrefix, new_uuid_v7};
use hearthsound_library::LibraryError;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::Authed;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::client_id;

/// Progress is published again after this many received bytes.
const PROGRESS_STEP: u64 = 1024 * 1024;

/// The name a received file takes inside its staging directory.
const STAGED_NAME: &str = "file";

/// The name an upload takes when the uploader gives no usable file name.
const FALLBACK_NAME: &str = "upload";

/// The outcome of one uploaded file.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadEntry {
    /// The upload's client id, `up_...`.
    pub upload_id: String,
    /// The file name, without any directory part.
    pub file_name: String,
    /// `completed` or `failed`.
    pub state: String,
    /// The track the file became, when it was completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    /// A stable code for a failure, such as `unsupported-media`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why the file failed, in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The answer to an upload request: one entry per `file` part, in order.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadResult {
    /// The uploaded files.
    pub uploads: Vec<UploadEntry>,
}

/// What one file needs while it is received: its identity, where it is
/// staged, and the limits of the request.
struct Incoming {
    /// The upload's client id, which events carry.
    client: String,
    /// The file name the uploader gave, without a directory part.
    file_name: String,
    /// The directory the file is staged in.
    dir: PathBuf,
    /// The largest file accepted, in bytes.
    max: u64,
    /// The size the request promised, reported as the upload's total.
    total: u64,
}

/// Accepts one or more audio files into the caller's library.
///
/// The permission and the `uploads.enabled` setting are checked before any
/// part is read. A file over `uploads.max_file_bytes` ends the request with
/// `too-large`; files completed before it stay in the library. A file the
/// library does not index answers as a failed entry instead of an error, so
/// the other files of the request still land.
///
/// # Parameters
///
/// - `authed`: the caller, who must hold `can_upload`.
/// - `state`: the shared services.
/// - `headers`: the request headers; `Content-Length` sets the total.
/// - `request`: the multipart body.
///
/// # Returns
///
/// `201 Created` with one entry per `file` part.
///
/// # Errors
///
/// Answers `permission-denied` (403) when uploads are off or the account may
/// not upload, `invalid-request` (400) when the body is not multipart, has no
/// `file` part or is malformed, `too-large` (413) when a file is over the
/// limit, and `internal` (500) when the library cannot take a file.
#[utoipa::path(
    post,
    path = "/api/v1/uploads",
    tag = "uploads",
    request_body(content_type = "multipart/form-data", description = "One or more `file` parts"),
    responses(
        (status = 201, description = "The files were received; each entry says what became of it", body = UploadResult),
        (status = 400, description = "The body is not multipart, has no `file` part, or is malformed"),
        (status = 403, description = "Uploads are off, or the account may not upload"),
        (status = 413, description = "A file is larger than uploads.max_file_bytes"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn upload(
    authed: Authed,
    State(state): State<NativeState>,
    headers: HeaderMap,
    request: Request,
) -> Result<(StatusCode, Json<UploadResult>), Problem> {
    let settings = state.settings().current();
    let enabled = settings
        .get(keys::UPLOADS_ENABLED)
        .map_err(|error| problem::internal("reading uploads.enabled", &error))?;
    if !enabled {
        return Err(Problem::permission_denied(
            "Uploads are turned off on this server.",
        ));
    }
    if !authed.principal.can_upload {
        return Err(Problem::permission_denied("This account may not upload."));
    }
    let max = settings
        .get(keys::UPLOADS_MAX_FILE_BYTES)
        .map_err(|error| problem::internal("reading uploads.max_file_bytes", &error))?;
    let max = u64::try_from(max).unwrap_or(0);
    let total = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    let mut multipart = Multipart::from_request(request, &state)
        .await
        .map_err(|rejection| Problem::invalid_request(rejection.body_text()))?;
    let mut uploads = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| Problem::invalid_request(error.body_text()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        uploads.push(receive_file(&state, authed.user_id(), &mut field, max, total).await?);
    }
    if uploads.is_empty() {
        return Err(Problem::invalid_request(
            "Send the files as multipart `file` parts.",
        ));
    }
    Ok((StatusCode::CREATED, Json(UploadResult { uploads })))
}

/// Receives one `file` part and removes its staging directory afterwards,
/// whatever the outcome.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `user`: the uploader's user id.
/// - `field`: the part to read; it is drained as far as the request needs.
/// - `max`: the largest accepted file, in bytes.
/// - `total`: the size the request promised.
///
/// # Returns
///
/// The entry for the file: completed, or failed with a stable error code.
///
/// # Errors
///
/// Returns the problem of the first failure that is not a per-file outcome.
async fn receive_file(
    state: &NativeState,
    user: Uuid,
    field: &mut Field<'_>,
    max: u64,
    total: u64,
) -> Result<UploadEntry, Problem> {
    let upload_id = new_uuid_v7();
    let incoming = Incoming {
        client: client_id(IdPrefix::Upload, upload_id),
        file_name: staged_file_name(field.file_name()),
        dir: state.services().uploads_staging.join(upload_id.to_string()),
        max,
        total,
    };
    let outcome = stage_and_ingest(state, user, field, &incoming).await;
    remove_staging(&incoming.dir).await;
    outcome
}

/// Writes one part into its staging directory and hands the file to ingest.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `user`: the uploader's user id.
/// - `field`: the part to read.
/// - `incoming`: the file's identity, staging directory and limits.
///
/// # Returns
///
/// The completed or failed entry of the file.
///
/// # Errors
///
/// Answers `too-large` when the part exceeds `max`, `invalid-request` when
/// the part cannot be read, and `internal` when a staged file cannot be
/// written or the library fails for a reason other than an unsupported file.
async fn stage_and_ingest(
    state: &NativeState,
    user: Uuid,
    field: &mut Field<'_>,
    incoming: &Incoming,
) -> Result<UploadEntry, Problem> {
    tokio::fs::create_dir_all(&incoming.dir)
        .await
        .map_err(|error| problem::internal("creating an upload's staging directory", &error))?;
    let path = incoming.dir.join(STAGED_NAME);
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|error| problem::internal("creating a staged upload", &error))?;
    publish(state, incoming, "receiving", 0);

    let mut received: u64 = 0;
    let mut reported: u64 = 0;
    loop {
        let chunk = match field.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                publish(state, incoming, "failed", received);
                return Err(Problem::invalid_request(error.body_text()));
            }
        };
        received = received.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if received > incoming.max {
            publish(state, incoming, "failed", received);
            return Err(Problem::too_large(format!(
                "{} is larger than uploads.max_file_bytes ({} bytes).",
                incoming.file_name, incoming.max
            )));
        }
        if let Err(error) = file.write_all(&chunk).await {
            publish(state, incoming, "failed", received);
            return Err(problem::internal("writing a staged upload", &error));
        }
        if received - reported >= PROGRESS_STEP {
            publish(state, incoming, "receiving", received);
            reported = received;
        }
    }
    if let Err(error) = file.flush().await {
        publish(state, incoming, "failed", received);
        return Err(problem::internal("flushing a staged upload", &error));
    }
    drop(file);

    publish(state, incoming, "processing", received);
    match state
        .services()
        .uploads
        .ingest(user, &path, &incoming.file_name)
        .await
    {
        Ok(ingested) => {
            publish(state, incoming, "completed", received);
            Ok(UploadEntry {
                upload_id: incoming.client.clone(),
                file_name: incoming.file_name.clone(),
                state: "completed".to_owned(),
                track_id: Some(ingested.track_id.to_string()),
                error: None,
                detail: None,
            })
        }
        Err(LibraryError::UnsupportedUpload { reason, .. }) => {
            publish(state, incoming, "failed", received);
            Ok(UploadEntry {
                upload_id: incoming.client.clone(),
                file_name: incoming.file_name.clone(),
                state: "failed".to_owned(),
                track_id: None,
                error: Some("unsupported-media".to_owned()),
                detail: Some(reason),
            })
        }
        Err(error) => {
            publish(state, incoming, "failed", received);
            Err(problem::internal("ingesting an uploaded file", &error))
        }
    }
}

/// Publishes one `upload.updated` event for a file.
///
/// # Parameters
///
/// - `state`: the shared services, whose bus carries the event.
/// - `incoming`: the file the event is about.
/// - `label`: the state the upload is in now.
/// - `received`: the bytes received so far.
fn publish(state: &NativeState, incoming: &Incoming, label: &str, received: u64) {
    state
        .services()
        .events
        .publish(ServerEvent::UploadUpdated(UploadUpdated {
            upload_id: incoming.client.clone(),
            state: label.to_owned(),
            bytes_received: received,
            total_bytes: incoming.total,
        }));
}

/// Returns the last path component of the name an uploader gave.
///
/// # Parameters
///
/// - `name`: the file name the part carries, if any.
///
/// # Returns
///
/// The last component after `/` or `\`, or `upload` when nothing is left.
fn staged_file_name(name: Option<&str>) -> String {
    let base = name
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    if base.is_empty() {
        FALLBACK_NAME.to_owned()
    } else {
        base.to_owned()
    }
}

/// Removes an upload's staging directory. A directory already gone is not an
/// error; any other failure is logged, since the upload's outcome stands.
///
/// # Parameters
///
/// - `dir`: the staging directory to remove.
async fn remove_staging(dir: &Path) {
    if let Err(error) = tokio::fs::remove_dir_all(dir).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %dir.display(), %error, "could not remove an upload's staging directory");
    }
}
