//! `/api/v1/admin/acquisitions`: the acquisition ledger, listed and steered.
//!
//! An administrator sees every user's acquisitions, not only their own, and
//! can cancel one that still holds work or retry one that failed or was
//! cancelled. A retry is a fresh attempt for the same track: it goes through
//! the same admission checks as any request, charged to the administrator who
//! asked for it.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use hearthsound_acquire::AcquireError;
use hearthsound_acquire::orchestrator::AcquireOutcome;
use hearthsound_acquire::repo::{AcquisitionSummary, ListFilter};
use hearthsound_core::{FailureReason, IdPrefix};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::auth::AdminAuthed;
use crate::extract::QueryParams;
use crate::pagination::{Cursor, PageParams, page};
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{client_id, parse_id};

/// The list filter a client may ask for.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AcquisitionFilter {
    /// `active` (the default) for acquisitions that still hold work,
    /// `recent` for ones that ended, or `all` for both.
    pub state: Option<String>,
}

/// One acquisition, as an administrator sees it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcquisitionView {
    /// The acquisition's client id (`ac_...`).
    pub id: String,
    /// The lifecycle state, such as `queued`, `downloading`, `completed` or
    /// `failed`.
    pub state: String,
    /// The priority the request was made at.
    pub priority: String,
    /// Whether the acquisition targets a track or a whole album: `track` or
    /// `album`.
    pub target_kind: String,
    /// The track's client id (`tr_...`), when the acquisition names one.
    pub track_id: Option<String>,
    /// The track's title, when known.
    pub track_title: Option<String>,
    /// The track's artist, when known.
    pub track_artist: Option<String>,
    /// The album's title, for an album target.
    pub album_title: Option<String>,
    /// The album's artist, for an album target that knows one.
    pub album_artist: Option<String>,
    /// The plugin instance doing the work, when one has been assigned.
    pub instance: Option<String>,
    /// Progress from 0 to 100, when the backend reports it.
    pub percent: Option<u8>,
    /// Why the acquisition failed, once it has.
    pub failure_reason: Option<String>,
    /// Whether the acquisition is queued and waiting for an acquisition source.
    ///
    /// A parked acquisition looks idle because its state is `queued`; this flag
    /// says it is not about to run.
    pub awaiting_source: bool,
    /// Why the acquisition is waiting for a source, when it is parked:
    /// `no-source` or `source-unavailable`.
    pub park_reason: Option<String>,
    /// When the acquisition was created, in unix milliseconds.
    pub created_at: i64,
    /// When the acquisition last changed, in unix milliseconds.
    pub updated_at: i64,
}

impl From<&AcquisitionSummary> for AcquisitionView {
    fn from(summary: &AcquisitionSummary) -> AcquisitionView {
        AcquisitionView {
            id: summary.client_id.clone(),
            state: summary.state.as_str().to_string(),
            priority: summary.priority.as_str().to_string(),
            target_kind: summary.target_kind.as_str().to_string(),
            track_id: summary.track_id.as_ref().map(ToString::to_string),
            track_title: summary.track_title.clone(),
            track_artist: summary.track_artist.clone(),
            album_title: summary.album_title.clone(),
            album_artist: summary.album_artist.clone(),
            instance: summary.instance.clone(),
            percent: summary.percent,
            failure_reason: summary
                .failure_reason
                .map(|reason| reason.as_str().to_string()),
            awaiting_source: summary.state == hearthsound_core::AcquisitionState::Queued
                && summary.park_reason.is_some(),
            park_reason: summary
                .park_reason
                .map(|reason| reason.as_str().to_string()),
            created_at: summary.created_at,
            updated_at: summary.updated_at,
        }
    }
}

/// One page of acquisitions.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcquisitionPage {
    /// The acquisitions on this page, newest first.
    pub acquisitions: Vec<AcquisitionView>,
    /// The cursor of the next page, absent on the last page.
    pub next_cursor: Option<String>,
}

/// The answer to a retry that started or joined an attempt.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetryView {
    /// The acquisition now doing the work (`ac_...`): the new attempt, or the
    /// one already running for the track that the retry joined.
    pub id: String,
    /// `started` when a new attempt was admitted, `joined` when an attempt
    /// already running for the track was reused.
    pub outcome: String,
}

/// The sort key of an acquisition: newest first, so an ascending key order is
/// the listing order.
fn position(summary: &AcquisitionSummary) -> Cursor {
    Cursor::new(
        format!("{:020}", i64::MAX - summary.created_at),
        summary.client_id.clone(),
    )
}

/// Maps a `state` filter onto a predicate over acquisitions that have ended.
///
/// # Errors
///
/// Answers `invalid-request` for a value other than `active`, `recent` or
/// `all`.
fn selects(filter: Option<&str>) -> Result<fn(&AcquisitionSummary) -> bool, Problem> {
    match filter.unwrap_or("active") {
        "active" => Ok(|row| !row.state.is_terminal()),
        "recent" => Ok(|row| row.state.is_terminal()),
        "all" => Ok(|_| true),
        _ => Err(Problem::invalid_request(
            "The state must be active, recent or all.",
        )),
    }
}

/// Lists acquisitions across every user, newest first.
///
/// `state=active` (the default) lists the acquisitions that still hold work,
/// `state=recent` the ones that ended, and `state=all` both. The ledger is
/// filtered and paged in memory, so a long-lived server with a large ledger
/// pays for the full read on every page.
///
/// # Errors
///
/// Answers `forbidden` for a caller who is not an administrator,
/// `invalid-request` for an unknown `state` or a malformed cursor, and
/// `internal` when the ledger cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/acquisitions",
    tag = "admin",
    params(AcquisitionFilter, PageParams),
    responses(
        (status = 200, description = "One page of acquisitions", body = AcquisitionPage),
        (status = 400, description = "Unknown state or malformed cursor", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not an administrator", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_acquisitions(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    QueryParams(filter): QueryParams<AcquisitionFilter>,
    QueryParams(params): QueryParams<PageParams>,
) -> Result<Json<AcquisitionPage>, Problem> {
    let keep = selects(filter.state.as_deref())?;
    let mut rows = state
        .services()
        .orchestrator
        .repository()
        .list(&ListFilter::default())
        .await
        .map_err(|error| problem::internal("listing acquisitions", &error))?;
    rows.retain(keep);
    rows.sort_by(|a, b| {
        let (a, b) = (position(a), position(b));
        (a.key, a.id).cmp(&(b.key, b.id))
    });
    let (rows, next_cursor) = page(rows, &params, position)?;
    Ok(Json(AcquisitionPage {
        acquisitions: rows.iter().map(AcquisitionView::from).collect(),
        next_cursor,
    }))
}

/// Cancels an acquisition that still holds work.
///
/// The worker is stopped and any side effects are undone; the row ends
/// `cancelled`.
///
/// # Errors
///
/// Answers `not-found` for an unknown id and `conflict` when the acquisition
/// has already ended.
#[utoipa::path(
    post,
    path = "/api/v1/admin/acquisitions/{id}/cancel",
    tag = "admin",
    params(("id" = String, Path, description = "The acquisition's client id")),
    responses(
        (status = 204, description = "The acquisition is cancelled"),
        (status = 403, description = "Not an administrator", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such acquisition", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The acquisition has already ended", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn cancel_acquisition(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&id, IdPrefix::Acquisition)?;
    let orchestrator = &state.services().orchestrator;
    let row = orchestrator
        .repository()
        .get(id)
        .await
        .map_err(|error| problem::internal("reading an acquisition", &error))?
        .ok_or_else(|| Problem::not_found("No such acquisition."))?;
    if row.state.is_terminal() {
        return Err(Problem::conflict(format!(
            "The acquisition is already {}.",
            row.state.as_str()
        )));
    }
    orchestrator
        .cancel(id)
        .await
        .map_err(|error| acquire_problem("cancelling an acquisition", error))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Starts a new attempt for a failed or cancelled acquisition.
///
/// The attempt is a forced request for the same track, so a track cooling
/// down after its failure is tried again. The caller's permission and daily
/// quota apply.
///
/// # Errors
///
/// Answers `not-found` for an unknown id, `conflict` when the acquisition is
/// not failed or cancelled or the track is already available, and
/// `permission-denied` or `quota-exceeded` when the administrator may not
/// acquire or has spent the day's allowance.
#[utoipa::path(
    post,
    path = "/api/v1/admin/acquisitions/{id}/retry",
    tag = "admin",
    params(("id" = String, Path, description = "The acquisition's client id")),
    responses(
        (status = 202, description = "A new attempt was started or joined", body = RetryView),
        (status = 403, description = "Not allowed to acquire, or over quota", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such acquisition", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The acquisition cannot be retried now", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn retry_acquisition(
    AdminAuthed(admin): AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<RetryView>), Problem> {
    let id = parse_id(&id, IdPrefix::Acquisition)?;
    let outcome = state
        .services()
        .orchestrator
        .retry(id, Some(admin.user_id()), admin.principal.can_acquire)
        .await
        .map_err(|error| acquire_problem("retrying an acquisition", error))?;
    match outcome {
        AcquireOutcome::Started(handle) => Ok((
            StatusCode::ACCEPTED,
            Json(RetryView {
                id: client_id(IdPrefix::Acquisition, handle.id),
                outcome: "started".to_string(),
            }),
        )),
        AcquireOutcome::Joined(handle) => Ok((
            StatusCode::ACCEPTED,
            Json(RetryView {
                id: client_id(IdPrefix::Acquisition, handle.id),
                outcome: "joined".to_string(),
            }),
        )),
        AcquireOutcome::AlreadyAvailable { .. } => {
            Err(Problem::conflict("The track is already available."))
        }
        AcquireOutcome::CoolingDown { .. } => Err(Problem::conflict("The track is cooling down.")),
        AcquireOutcome::Denied { reason, .. } => Err(denied_problem(reason)),
    }
}

/// Maps an acquisition error onto the problem a client sees.
///
/// # Parameters
///
/// - `context`: what was being attempted, for the log line of an internal error.
/// - `error`: the error the orchestrator returned.
///
/// # Returns
///
/// `not-found` for an unknown acquisition, `conflict` for one that cannot be
/// retried, and `internal` for anything else.
fn acquire_problem(context: &str, error: AcquireError) -> Problem {
    match error {
        AcquireError::UnknownAcquisition(_) => Problem::not_found("No such acquisition."),
        AcquireError::NotRetryable { state, .. } => Problem::conflict(format!(
            "The acquisition is {} and cannot be retried.",
            state.as_str()
        )),
        other => problem::internal(context, &other),
    }
}

/// Maps a refused admission onto the problem that explains it.
///
/// # Parameters
///
/// - `reason`: why the request was refused.
///
/// # Returns
///
/// `permission-denied` or `quota-exceeded` for those two reasons, and
/// `conflict` for any other.
fn denied_problem(reason: FailureReason) -> Problem {
    match reason {
        FailureReason::PermissionDenied => {
            Problem::permission_denied("Your account may not acquire tracks.")
        }
        FailureReason::QuotaExceeded => {
            Problem::quota_exceeded("You have spent today's acquisition allowance.")
        }
        other => Problem::conflict(format!("The retry was refused: {}.", other.as_str())),
    }
}
