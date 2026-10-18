//! The acquisition vendor endpoints.
//!
//! `hsGetAvailability` answers what the server knows about the tracks and albums
//! a client names, without starting anything: the availability the library
//! stores, whether the requesting account's personal library holds the entity,
//! and the acquisition in flight for it, if any.
//!
//! Every endpoint here belongs to the `hearthsound` vendor extension, so each one
//! is answered only when the request carries `hearthsound=1` and answers Subsonic
//! error 70 without it: a client that did not opt in must not see a feature the
//! base protocol does not have.

use std::collections::HashMap;

use hearthsound_acquire::repo::active_for_tracks;
use hearthsound_core::{EntityId, EntityKind, IdPrefix};

use crate::auth::Call;
use crate::browse::failure;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};
use crate::vendor;

/// The operation name an availability read reports.
const AVAILABILITY_OPERATION: &str = "hsGetAvailability";

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
                let acquisition = EntityId::from_uuid(IdPrefix::Acquisition, row.id);
                entry = entry.set("acquisitionId", acquisition.to_string());
                if let Some(orchestrator) = state.acquisitions() {
                    match orchestrator.progress_of(row.id).await {
                        Ok(Some(progress)) => {
                            if let Some(percent) = progress.percent {
                                entry = entry.set("percent", i64::from(percent));
                            }
                            if let Some(eta) = progress.eta_secs {
                                entry = entry.set("etaSecs", i64::from(eta));
                            }
                        }
                        Ok(None) => {}
                        Err(error) => return Err(failure(AVAILABILITY_OPERATION, &error)),
                    }
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
