//! The acquisition vendor endpoints.
//!
//! `hsGetAvailability` answers what the server knows about the tracks and albums
//! a client names, without starting anything: the availability the library
//! stores, whether the requesting account's personal library holds the entity,
//! and the acquisition in flight for it, if any. `hsAcquire` starts what a
//! client asks for: a track is requested on its own and an album is requested
//! track by track, and every row of the answer is the acquisition the request
//! produced, the acquisition it attached to, or the last acquisition the track
//! has. `hsGetAcquisitions` lists the ledger - one account's own acquisitions,
//! or every account's for an administrator - and `hsCancelAcquisition` ends one.
//!
//! Every endpoint here belongs to the `hearthsound` vendor extension, so each one
//! is answered only when the request carries `hearthsound=1` and answers Subsonic
//! error 70 without it: a client that did not opt in must not see a feature the
//! base protocol does not have.

use std::collections::HashMap;

use hearthsound_acquire::AcquireError;
use hearthsound_acquire::orchestrator::{
    AcquireOutcome, AcquireRequest, AcquisitionOrchestrator, AlbumAcquireRequest,
};
use hearthsound_acquire::repo::{
    AcquisitionRow, LedgerQuery, LedgerWindow, active_for_tracks, latest_for_tracks,
};
use hearthsound_core::{ClientId, EntityId, EntityKind, FailureReason, IdPrefix, Layer, Priority};
use uuid::Uuid;

use crate::auth::Call;
use crate::browse::failure;
use crate::catalog;
use crate::endpoints::browse::number;
use crate::params::now_millis;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::state::SubsonicState;
use crate::value::{Array, Object};
use crate::vendor;

/// The operation name an availability read reports.
const AVAILABILITY_OPERATION: &str = "hsGetAvailability";

/// The operation name an acquisition request reports.
const ACQUIRE_OPERATION: &str = "hsAcquire";

/// The operation name an acquisition listing reports.
const LIST_OPERATION: &str = "hsGetAcquisitions";

/// The operation name a cancellation reports.
const CANCEL_OPERATION: &str = "hsCancelAcquisition";

/// The reason an acquisition started by the vendor extension records.
const ACQUIRE_REASON: &str = "vendor";

/// How long a terminal acquisition stays in the recent window.
///
/// The window is the plan's "recent" period: a client that reconnects asks for
/// what finished since it last looked, and a day is long enough for that while
/// still keeping the answer small.
const RECENT_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// How many rows a listing answers when the request names no count.
const DEFAULT_COUNT: u32 = 50;

/// The largest number of rows a listing answers.
const MAX_COUNT: u32 = 500;

/// Answers `hsGetAvailability`: what the server knows about the named entities.
///
/// One entry is answered per requested id, in request order: the entity's
/// availability state and whether the requesting account's personal library holds
/// it, plus the acquisition in flight for a track, if there is one. An album's
/// entry aggregates its member tracks and carries no acquisition fields, and an
/// `x_` catalog id is reported from the stored row it resolved to, or as
/// `remote` when nothing has materialized it.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` may be repeated and names tracks and
///   albums, stored or catalog ids; the request must carry `hearthsound=1`.
///
/// # Returns
///
/// An envelope carrying `availabilities.availability`, or a failed envelope. An
/// id that names nothing - a stored id whose entity is gone, a repeated entry
/// that no longer exists - is left out of the list, so a client can tell "not
/// known here" from a state.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// error 10 when it names no id, and a generic error when a read fails.
pub async fn hs_get_availability(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let ids = requested(&call);
    if ids.is_empty() {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    }
    let entries = match availabilities(&call, &ids).await {
        Ok(entries) => entries,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.encode(Response::ok().member(
        "availabilities",
        Object::new().set("availability", Array::new(entries)),
    ))
}

/// Returns the ids a request names, in request order.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The values of the `id` parameter, with empty values left out and a repeated
/// id kept once per occurrence.
fn requested(call: &Call) -> Vec<String> {
    call.params()
        .get_all("id")
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Builds one availability entry per requested id.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `ids`: the ids the request named, in request order.
///
/// # Returns
///
/// The entries, in request order.
///
/// # Errors
///
/// Returns a generic error when a read fails.
async fn availabilities(call: &Call, ids: &[String]) -> Result<Vec<Object>, SubsonicError> {
    let state = call.state();
    let user = call.principal().user_id.uuid();
    let mut subjects = HashMap::new();
    for id in ids {
        if let Some(subject) = vendor::resolve(state, id).await? {
            subjects.insert(id.clone(), subject);
        }
    }
    let facts = vendor::facts(state, user, &subjects).await?;
    let tracks: Vec<EntityId> = subjects
        .values()
        .filter(|subject| subject.kind == EntityKind::Track)
        .filter_map(|subject| subject.entity)
        .collect();
    let active = {
        let mut conn = state
            .database()
            .readers()
            .acquire()
            .await
            .map_err(|error| failure(AVAILABILITY_OPERATION, &error))?;
        active_for_tracks(&mut conn, &tracks)
            .await
            .map_err(|error| failure(AVAILABILITY_OPERATION, &error))?
    };
    let mut entries = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(subject) = subjects.get(id) else {
            continue;
        };
        // Artists are not part of this endpoint: a client asks about an album's
        // availability, not an artist's, and the answer would aggregate over
        // every track the artist is credited on.
        if !matches!(subject.kind, EntityKind::Track | EntityKind::Album) {
            continue;
        }
        let Some(standing) = vendor::standing(&facts, subject) else {
            continue;
        };
        let mut entry = Object::new()
            .set("id", id.clone())
            .set("state", standing.availability.as_str())
            .set("inLibrary", standing.in_library);
        if subject.kind == EntityKind::Track {
            if let Some(row) = standing.entity.and_then(|track| active.get(&track)) {
                entry = entry.set("acquisitionId", acquisition_id(row.id));
                if let Some(percent) = row.percent() {
                    entry = entry.set("percent", i64::from(percent));
                }
                if let Some(eta) = row.eta_secs() {
                    entry = entry.set("etaSecs", i64::from(eta));
                }
                if let Some(reason) = row.park_reason {
                    entry = entry.set("awaitingSource", true);
                    entry = entry.set("parkReason", reason.as_str());
                }
            }
        } else if standing.members.is_empty() && standing.entity.is_some() {
            // An album's state aggregates its member tracks, so an album that
            // holds none says nothing: an unmaterialized catalog album is
            // answered as `remote` above, and a stored album with no tracks is
            // left out the way an unknown stored id is.
            continue;
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Answers `hsAcquire`: asks the acquisition layer for the named entities.
///
/// A track is requested on its own, an album is requested track by track, and an
/// `x_` catalog id is materialized first, so the acquisition has a stored target
/// to import into. Every row of the answer is the acquisition the request
/// admitted, the acquisition it attached to, or - for a track that already has a
/// file or is cooling down - the last acquisition the track has; an id with no
/// such row is left out.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` may be repeated and names tracks and
///   albums, stored or catalog ids; `priority` is `play`, `next`, `queue` or
///   `background` and defaults to `queue`; `retry` (`1`, `true` or `yes`) makes
///   the request ignore a track's cooldown and start a fresh attempt. The request
///   must carry `hearthsound=1`, and the account must hold `can_acquire`.
///
/// # Returns
///
/// An envelope carrying `acquisitions.acquisition`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1` and
/// when an id names nothing this endpoint can acquire, error 10 when the request
/// names no id, error 50 when the account may not acquire and when admission
/// denies one of the requests, and a generic error when a request cannot be read
/// or written. The error carries the stored reason in `hearthsoundReason`
/// (`quota-exceeded` or `permission-denied`) for a client that sent
/// `hearthsound=1`. A request that is denied or fails mid-list leaves the
/// acquisitions the earlier ids produced in the ledger.
pub async fn hs_acquire(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let ids = requested(&call);
    if ids.is_empty() {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    }
    if !call.principal().can_acquire {
        return call.encode(Response::failed(
            SubsonicError::not_authorized().with_reason(FailureReason::PermissionDenied.as_str()),
        ));
    }
    let priority = match priority(&call) {
        Ok(priority) => priority,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let retry = retry(&call);
    let state = call.state();
    let Some(orchestrator) = state.acquisitions() else {
        tracing::error!("the /rest surface has no acquisition layer to admit an acquisition");
        return call.encode(Response::failed(SubsonicError::generic()));
    };
    let instances = instance_slugs(state);
    let user = call.principal().user_id.uuid();
    let can_acquire = call.principal().can_acquire;
    let mut rows = Vec::new();
    for id in &ids {
        let outcomes =
            match acquire(state, orchestrator, user, can_acquire, id, priority, retry).await {
                Ok(outcomes) => outcomes,
                Err(error) => return call.encode(Response::failed(error)),
            };
        for outcome in outcomes {
            if let Err(error) = record(state, orchestrator, outcome, &instances, &mut rows).await {
                return call.encode(Response::failed(error));
            }
        }
    }
    call.encode(Response::ok().member(
        "acquisitions",
        Object::new().set("acquisition", Array::new(rows)),
    ))
}

/// Reads the `priority` parameter.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The priority, which is `queue` when the request did not carry one.
///
/// # Errors
///
/// Returns a generic error when the value is not one of `play`, `next`, `queue`
/// or `background`.
fn priority(call: &Call) -> Result<Priority, SubsonicError> {
    let Some(value) = call.get("priority").map(str::trim) else {
        return Ok(Priority::Queue);
    };
    if value.is_empty() {
        return Ok(Priority::Queue);
    }
    value.parse::<Priority>().map_err(|_| {
        tracing::debug!(
            value,
            "a vendor acquire request carried an unknown priority"
        );
        SubsonicError::new(
            codes::GENERIC,
            "The `priority` parameter must be one of play, next, queue or background.",
        )
    })
}

/// Reads the `retry` parameter.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// Whether the request asks the acquisition layer to ignore the track's cooldown.
/// `1`, `true` and `yes` ask for it, in any case; a missing or other value does
/// not.
fn retry(call: &Call) -> bool {
    call.get("retry").map(str::trim).is_some_and(|value| {
        value.eq_ignore_ascii_case("1")
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
    })
}

/// Requests one id and reports what admission decided.
///
/// # Parameters
///
/// - `state`: the endpoint state.
/// - `orchestrator`: the acquisition layer.
/// - `user`: the account the request is made on behalf of.
/// - `can_acquire`: whether that account may acquire.
/// - `raw`: the id the client wrote.
/// - `priority`: the priority every track request carries.
/// - `retry`: whether the request ignores a track's cooldown.
///
/// # Returns
///
/// The outcome of a track request, or the outcomes of an album's fan-out, in
/// playing order.
///
/// # Errors
///
/// Answers Subsonic error 70 when the id names something this endpoint does not
/// acquire, and a generic error otherwise.
async fn acquire(
    state: &SubsonicState,
    orchestrator: &AcquisitionOrchestrator,
    user: Uuid,
    can_acquire: bool,
    raw: &str,
    priority: Priority,
    retry: bool,
) -> Result<Vec<AcquireOutcome>, SubsonicError> {
    let Ok(client) = ClientId::parse(raw) else {
        return Err(SubsonicError::not_found());
    };
    match client {
        ClientId::Entity(id) => match id.kind().entity_kind() {
            Some(EntityKind::Track) => {
                request_track(orchestrator, user, can_acquire, id, priority, retry)
                    .await
                    .map(|outcome| vec![outcome])
            }
            Some(EntityKind::Album) => {
                request_album(orchestrator, user, can_acquire, id, priority, retry).await
            }
            _ => Err(SubsonicError::not_found()),
        },
        ClientId::Catalog(id) => match id.kind() {
            EntityKind::Track => {
                let track = state
                    .catalog()
                    .materialize_track(&id)
                    .await
                    .map_err(|error| catalog::failure(&error))?;
                request_track(orchestrator, user, can_acquire, track, priority, retry)
                    .await
                    .map(|outcome| vec![outcome])
            }
            EntityKind::Album => {
                let album = state
                    .catalog()
                    .materialize_album(&id)
                    .await
                    .map_err(|error| catalog::failure(&error))?;
                request_album(
                    orchestrator,
                    user,
                    can_acquire,
                    album.album,
                    priority,
                    retry,
                )
                .await
            }
            _ => Err(SubsonicError::not_found()),
        },
    }
}

/// Requests one stored track.
///
/// # Errors
///
/// Answers Subsonic error 70 when the track has no row, and a generic error when
/// the request cannot be read or written.
async fn request_track(
    orchestrator: &AcquisitionOrchestrator,
    user: Uuid,
    can_acquire: bool,
    track: EntityId,
    priority: Priority,
    retry: bool,
) -> Result<AcquireOutcome, SubsonicError> {
    let request = AcquireRequest::new(track, priority, ACQUIRE_REASON)
        .with_requested_by(Some(user))
        .with_can_acquire(can_acquire)
        .forced_if(retry);
    orchestrator
        .request(request)
        .await
        .map_err(|error| acquire_failure(&error))
}

/// Requests every track of one stored album.
///
/// # Errors
///
/// Answers Subsonic error 70 when the album has no tracks, and a generic error
/// when a request cannot be read or written.
async fn request_album(
    orchestrator: &AcquisitionOrchestrator,
    user: Uuid,
    can_acquire: bool,
    album: EntityId,
    priority: Priority,
    retry: bool,
) -> Result<Vec<AcquireOutcome>, SubsonicError> {
    let request = AlbumAcquireRequest {
        album_id: album,
        priority,
        reason: ACQUIRE_REASON.to_owned(),
        requested_by: Some(user),
        can_acquire,
        force: retry,
    };
    orchestrator
        .request_album(request)
        .await
        .map(|outcomes| {
            outcomes
                .into_iter()
                .map(|outcome| outcome.outcome)
                .collect()
        })
        .map_err(|error| acquire_failure(&error))
}

/// Maps an acquisition failure onto the failure envelope.
///
/// # Parameters
///
/// - `error`: the failure a request returned.
///
/// # Returns
///
/// Subsonic error 70 when the request named an entity that does not exist, and a
/// generic error otherwise: a database that refused a write is nothing a client
/// can act on, while an operator reading the log wants the cause.
fn acquire_failure(error: &AcquireError) -> SubsonicError {
    match error {
        AcquireError::UnknownTrack(_) | AcquireError::UnknownAlbum(_) => {
            tracing::debug!(error = %error, "a vendor acquire request named an unknown entity");
            SubsonicError::not_found()
        }
        other => failure(ACQUIRE_OPERATION, other),
    }
}

/// Records what one request produced as the rows of the answer.
///
/// # Parameters
///
/// - `state`: the endpoint state.
/// - `orchestrator`: the acquisition layer.
/// - `outcome`: what admission decided for one track.
/// - `instances`: the instance slugs the registry serves, keyed by id.
/// - `rows`: the answer's rows, which a row is appended to.
///
/// # Errors
///
/// Answers Subsonic error 50 when admission denied the request - carrying the
/// stored reason in `hearthsoundReason` (`quota-exceeded` or `permission-denied`)
/// - and a generic error when the ledger cannot be read.
async fn record(
    state: &SubsonicState,
    orchestrator: &AcquisitionOrchestrator,
    outcome: AcquireOutcome,
    instances: &HashMap<Uuid, String>,
    rows: &mut Vec<Object>,
) -> Result<(), SubsonicError> {
    match outcome {
        AcquireOutcome::Started(handle) | AcquireOutcome::Joined(handle) => {
            let row = orchestrator
                .repository()
                .get(handle.id)
                .await
                .map_err(|error| failure(ACQUIRE_OPERATION, &error))?;
            if let Some(row) = row {
                rows.push(acquisition_row(&row, instances));
            }
            Ok(())
        }
        AcquireOutcome::AlreadyAvailable { track_id }
        | AcquireOutcome::CoolingDown { track_id, .. } => {
            let latest = {
                let mut conn = state
                    .database()
                    .readers()
                    .acquire()
                    .await
                    .map_err(|error| failure(ACQUIRE_OPERATION, &error))?;
                latest_for_tracks(&mut conn, &[track_id])
                    .await
                    .map_err(|error| failure(ACQUIRE_OPERATION, &error))?
            };
            if let Some(row) = latest.get(&track_id) {
                rows.push(acquisition_row(row, instances));
            }
            Ok(())
        }
        AcquireOutcome::Denied { reason, .. } => {
            tracing::debug!(
                reason = reason.as_str(),
                "admission denied a vendor acquire request"
            );
            Err(SubsonicError::not_authorized().with_reason(reason.as_str()))
        }
    }
}

/// Builds one row of an acquisition list.
///
/// # Parameters
///
/// - `row`: the ledger row.
/// - `instances`: the instance slugs the registry serves, keyed by id.
///
/// # Returns
///
/// The row, in the order the vendor extension documents: its client id, its
/// target, its state and priority, and the optional progress, instance and
/// failure.
fn acquisition_row(row: &AcquisitionRow, instances: &HashMap<Uuid, String>) -> Object {
    let mut object = Object::new()
        .set("id", acquisition_id(row.id))
        .set("targetId", target_id(row))
        .set("state", row.state.as_str())
        .set("priority", row.priority.as_str());
    if let Some(percent) = row.percent() {
        object = object.set("percent", i64::from(percent));
    }
    if let Some(eta) = row.eta_secs() {
        object = object.set("etaSecs", i64::from(eta));
    }
    if let Some(instance) = row.instance_id.and_then(|id| instances.get(&id)) {
        object = object.set("instance", instance.clone());
    }

    if let Some(reason) = row.failure_reason {
        object = object.set("failureReason", reason.as_str());
    }
    if let Some(reason) = row.park_reason {
        object = object.set("awaitingSource", true);
        object = object.set("parkReason", reason.as_str());
    }
    object
}

/// Returns the client id of an acquisition.
fn acquisition_id(id: Uuid) -> String {
    EntityId::from_uuid(IdPrefix::Acquisition, id).to_string()
}

/// Returns the client id of the track or album an acquisition targets.
fn target_id(row: &AcquisitionRow) -> String {
    row.track_id
        .or(row.album_id)
        .map(|id| id.to_string())
        .unwrap_or_default()
}

/// Returns the slugs of the acquisition instances the server serves.
///
/// # Parameters
///
/// - `state`: the endpoint state.
///
/// # Returns
///
/// The slug of each instance of the acquisition layer, keyed by its id. A state
/// built without a registry has none, so an acquisition reports no instance.
fn instance_slugs(state: &SubsonicState) -> HashMap<Uuid, String> {
    state
        .registry()
        .map(|registry| {
            registry
                .layers(Layer::Acquisition)
                .into_iter()
                .map(|instance| (instance.id(), instance.slug().as_str().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// Answers `hsGetAcquisitions`: one page of the acquisition ledger.
///
/// A non-administrator sees only the acquisitions their own account requested; an
/// administrator sees every account's. The window is the plan's three views:
/// `active` is the acquisitions that still hold work, `recent` the ones that
/// ended within the last day, and `all` both.
///
/// # Parameters
///
/// - `call`: the authenticated call. `state` is `active`, `recent` or `all` and
///   defaults to `active`; `count` is the page size and defaults to 50, at most
///   500; `offset` skips that many rows. The request must carry `hearthsound=1`.
///
/// # Returns
///
/// An envelope carrying `acquisitions.acquisition`, newest first, or a failed
/// envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// and a generic error when `state`, `count` or `offset` is malformed and when
/// the ledger cannot be read.
pub async fn hs_get_acquisitions(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let window = match window(&call) {
        Ok(window) => window,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let query = match page(&call, window) {
        Ok(query) => query,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let state = call.state();
    let Some(orchestrator) = state.acquisitions() else {
        tracing::error!("the /rest surface has no acquisition layer to list acquisitions");
        return call.encode(Response::failed(SubsonicError::generic()));
    };
    let rows = match orchestrator.list_window(&query).await {
        Ok(rows) => rows,
        Err(error) => return call.encode(Response::failed(failure(LIST_OPERATION, &error))),
    };
    let instances = instance_slugs(state);
    let entries: Vec<Object> = rows
        .iter()
        .map(|row| acquisition_row(row, &instances))
        .collect();
    call.encode(Response::ok().member(
        "acquisitions",
        Object::new().set("acquisition", Array::new(entries)),
    ))
}

/// Answers `hsCancelAcquisition`: ends one acquisition.
///
/// Cancelling is idempotent: an acquisition that already ended answers an empty
/// success, because what the client asked for - that it is not running - holds.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names the acquisition by its client id.
///   The request must carry `hearthsound=1`.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// error 10 when it names no id, error 70 when the id names no acquisition,
/// error 50 when a non-administrator cancels an acquisition another account
/// requested, and a generic error when the cancellation fails.
pub async fn hs_cancel_acquisition(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let Some(raw) = call
        .get("id")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let Some(acquisition) = acquisition_of(raw) else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };
    let state = call.state();
    let Some(orchestrator) = state.acquisitions() else {
        tracing::error!("the /rest surface has no acquisition layer to cancel an acquisition");
        return call.encode(Response::failed(SubsonicError::generic()));
    };
    let principal = call.principal();
    let listed = match orchestrator.progress_of(acquisition).await {
        Ok(Some(progress)) => progress,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => {
            return call.encode(Response::failed(failure(CANCEL_OPERATION, &error)));
        }
    };
    if !principal.is_admin && listed.requested_by != Some(principal.user_id.uuid()) {
        tracing::debug!(
            acquisition = %acquisition,
            user = %principal.username,
            "refused a cancellation of another account's acquisition"
        );
        return call.encode(Response::failed(SubsonicError::not_authorized()));
    }
    match orchestrator.cancel(acquisition).await {
        Ok(()) => call.encode(Response::ok()),
        Err(AcquireError::UnknownAcquisition(_)) => {
            call.encode(Response::failed(SubsonicError::not_found()))
        }
        Err(error) => call.encode(Response::failed(failure(CANCEL_OPERATION, &error))),
    }
}

/// Reads the `state` parameter into the ledger window it names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The window, which is `active` when the request did not carry one.
///
/// # Errors
///
/// Returns a generic error when the value is not one of `active`, `recent` or
/// `all`.
fn window(call: &Call) -> Result<LedgerWindow, SubsonicError> {
    match call.get("state").map(str::trim) {
        None | Some("") | Some("active") => Ok(LedgerWindow::Active),
        Some("recent") => Ok(LedgerWindow::Recent {
            since: now_millis() - RECENT_WINDOW_MS,
        }),
        Some("all") => Ok(LedgerWindow::All),
        Some(value) => {
            tracing::debug!(value, "a vendor listing carried an unknown state");
            Err(SubsonicError::new(
                codes::GENERIC,
                "The `state` parameter must be one of active, recent or all.",
            ))
        }
    }
}

/// Builds the ledger query a listing reads.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `count` and `offset` are read.
/// - `window`: the window the `state` parameter named.
///
/// # Returns
///
/// The query: the window, the requester the principal may see, and the page.
///
/// # Errors
///
/// Returns a generic error when `count` or `offset` is not a number.
fn page(call: &Call, window: LedgerWindow) -> Result<LedgerQuery, SubsonicError> {
    let count = number(call, "count", i64::from(DEFAULT_COUNT))?;
    let offset = number(call, "offset", 0)?;
    let principal = call.principal();
    Ok(LedgerQuery {
        window,
        requested_by: (!principal.is_admin).then(|| principal.user_id.uuid()),
        limit: u32::try_from(count.clamp(0, i64::from(MAX_COUNT))).unwrap_or(MAX_COUNT),
        offset: u32::try_from(offset.max(0)).unwrap_or(u32::MAX),
    })
}

/// Returns the acquisition a client id names.
///
/// # Parameters
///
/// - `raw`: the id the client wrote.
///
/// # Returns
///
/// The acquisition's id, or `None` when the value is not an `ac_` client id.
fn acquisition_of(raw: &str) -> Option<Uuid> {
    match ClientId::parse(raw) {
        Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Acquisition => Some(id.uuid()),
        _ => None,
    }
}
