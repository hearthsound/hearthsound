//! The `getScanStatus` and `startScan` endpoints: what the library scanner is
//! doing, and how a client asks for a scan.
//!
//! A scan walks every library folder and indexes what it finds. A client that
//! starts one is answered with the status it just changed, and a client that
//! polls the status is told whether a scan is queued or running, how many files
//! it has dealt with, and when the folders were last scanned.

use hearthsound_library::query;
use hearthsound_library::scan::ScanRequest;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::Object;
use crate::{browse, projection};

/// Answers `getScanStatus`: what the library scanner is doing.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `scanStatus`, or a failed envelope.
///
/// # Errors
///
/// Answers a generic error when a read fails.
pub async fn get_scan_status(call: Call) -> Encoded {
    let status = match scan_status(&call).await {
        Ok(status) => status,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.encode(Response::ok().member("scanStatus", status))
}

/// Answers `startScan`: enqueues a scan of every library folder.
///
/// # Parameters
///
/// - `call`: the authenticated call. `fullScan` asks for every file to be
///   probed, ignoring the stored size and modification time.
///
/// # Returns
///
/// A successful envelope carrying the `scanStatus` the request started, or a
/// failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 50 when the caller is not an administrator, error 0
/// when `fullScan` is not a boolean or when this process does not scan, and a
/// generic error when the scan cannot be enqueued.
pub async fn start_scan(call: Call) -> Encoded {
    if !call.principal().is_admin {
        tracing::debug!("a non-administrator asked to start a scan");
        return call.encode(Response::failed(SubsonicError::not_authorized()));
    }
    let full = match call.get("fullScan").filter(|value| !value.is_empty()) {
        Some(value) => match value.parse::<bool>() {
            Ok(full) => full,
            Err(_) => {
                return call.encode(Response::failed(SubsonicError::new(
                    crate::response::codes::GENERIC,
                    "`fullScan` is true or false.",
                )));
            }
        },
        None => false,
    };
    let (Some(scanner), Some(jobs)) = (call.state().scanner(), call.state().jobs()) else {
        tracing::debug!("a scan was requested in a process that does not scan");
        return call.encode(Response::failed(SubsonicError::new(
            crate::response::codes::GENERIC,
            "This server does not scan its library.",
        )));
    };

    let request = if full {
        ScanRequest::all().full()
    } else {
        ScanRequest::all()
    };
    if let Err(error) = scanner.request_scan(jobs, &request).await {
        return call.encode(Response::failed(browse::failure("startScan", &error)));
    }

    // The request is counted from the moment it is enqueued, so the status the
    // caller is answered with reports the scan it just started.
    let status = match scan_status(&call).await {
        Ok(status) => status,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.encode(Response::ok().member("scanStatus", status))
}

/// Builds the `scanStatus` member.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The status: whether a scan is queued or running, how many files the running
/// scans have dealt with (or how many the library holds between scans), how many
/// library folders there are, and when they were last scanned.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn scan_status(call: &Call) -> Result<Object, SubsonicError> {
    let mut conn = match call.state().database().readers().acquire().await {
        Ok(conn) => conn,
        Err(error) => return Err(browse::failure("getScanStatus", &error)),
    };
    let stored = match query::count_folder_files(&mut conn).await {
        Ok(counted) => counted,
        Err(error) => return Err(browse::failure("getScanStatus", &error)),
    };
    let folders = match query::count_folders(&mut conn).await {
        Ok(counted) => counted,
        Err(error) => return Err(browse::failure("getScanStatus", &error)),
    };
    let last_scan = match query::last_scan(&mut conn).await {
        Ok(scanned) => scanned,
        Err(error) => return Err(browse::failure("getScanStatus", &error)),
    };
    drop(conn);

    let status = call.state().scanner().map(|scanner| scanner.status());
    let scanning = status.as_ref().is_some_and(|status| status.scanning);
    let counted = match status {
        Some(status) if status.scanning => {
            i64::try_from(status.files_processed).unwrap_or(i64::MAX)
        }
        _ => stored,
    };
    let object = Object::new()
        .set("scanning", scanning)
        .set("count", counted)
        .set("folderCount", folders);
    Ok(match last_scan {
        Some(at) => object.set("lastScan", projection::timestamp(at)),
        None => object,
    })
}
