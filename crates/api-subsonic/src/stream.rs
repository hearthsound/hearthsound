//! The stream service: the bytes behind `/rest/stream`.
//!
//! [`StreamService`] turns one [`StreamRequest`] into the HTTP response a
//! Subsonic client plays. A request for a stored track resolves to the track's
//! preferred media file, and that file is served straight from disk with the
//! HTTP semantics a seeking client needs: `Accept-Ranges`, `Range` and
//! `Content-Range`, `ETag` and `Last-Modified`, `If-None-Match`,
//! `If-Modified-Since`, `If-Range`, `HEAD`, and the codec's content type.
//!
//! # Two deliveries
//!
//! A request that can be answered with the stored file is served straight from
//! disk. The bytes come from tower-http's `ServeFile`, which opens the file,
//! answers a single range with `206`, answers an unsatisfiable range with
//! `416`, and never holds the whole file in memory. Everything that is
//! not a plain range - the validators and the conditional requests built on
//! them - is decided here, because the `ETag` is derived from the stored media
//! file (its size, its modification time, and its id) rather than from the path
//! alone, so a client that replaces a file revalidates instead of playing a
//! stale cache entry.
//!
//! A request that asks for another format, for a bitrate cap below the file's
//! own, or for an offset into the track is answered by one ffmpeg process
//! instead. A transcode is not the stored representation, so it carries no
//! validators and no ranges: it is answered with `200`, `Accept-Ranges: none`,
//! and the encoded bytes as they are produced, chunked and never with a
//! declared length. The first encoded chunk is awaited before the status is
//! committed, so an encoder that fails to start is answered `500`.
//!
//! # Pending streams
//!
//! A request for a track the server has not stored yet - a stored track without
//! a file - is not answered with an error while the track can still be
//! acquired. The service asks the acquisition layer for it through
//! [`PendingSource`], and answers according to `stream.pending.mode`:
//!
//! - `hold`, the default, sends nothing until the file is ready or
//!   `stream.pending.max_wait_secs` elapses, then serves the file normally;
//! - `fail` answers `503` with `Retry-After` at once and lets the acquisition
//!   continue in the background.
//!
//! An acquisition that is parked for want of an acquisition source - no plugin
//! instance can serve it - is answered `503` with `Retry-After` in either mode,
//! because waiting for an administrator to add one is not something a client's
//! connection should cover. That holds for an acquisition that parks while the
//! request is already held as well: the hold ends the moment it parks.
//!
//! Neither mode writes a byte before the outcome is known, so a failure, a
//! cooldown, or an elapsed wait is still answered as a status code: `hold`
//! answers `404` when the acquisition fails and `503` when its wait elapses.
//!
//! # A stream is a session
//!
//! Serving bytes registers a stream session in the activity service's
//! [`StreamSessions`] registry, with the audio-delivered flag set the first
//! time a body frame is written to the client. That flag is what lets a later
//! `scrobble` refuse to record a play for a stream the client never heard (see
//! `hearthsound_activity::ActivityService::scrobble`), so it is set by the body
//! rather than when the response is built: a client that gave up while the file
//! was still being acquired records no play. Recording the file's last access is
//! a detached write: it never delays the first byte, and a failure is logged
//! and dropped, because it is bookkeeping for the retention policy rather than
//! part of the response.
//!
//! # Failures are status codes
//!
//! A stream never answers a Subsonic envelope (invariant 5 of the repository
//! rules): a client that treats any 200 body as audio would play an error
//! document as noise. [`StreamError`] therefore carries the HTTP status to
//! answer with - `404` for an id that names no track and for a track that is
//! not and cannot be stored, `403` for a requester who may not acquire the
//! track, `503` with a `Retry-After` while an acquisition is still running, and
//! `500` for a file or a database the server could not read.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{StreamExt, TryStream, TryStreamExt, stream};
use hearthsound_acquire::AcquireError;
use hearthsound_acquire::orchestrator::{
    AcquireOutcome, AcquireRequest, AcquisitionHandle, AcquisitionOrchestrator, AcquisitionProgress,
};
use hearthsound_activity::{AudioDelivered, StreamSessions};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingKey, SettingType, SettingsStore, keys};
use hearthsound_core::{EntityId, FailureReason, IdPrefix, Priority};
use hearthsound_db::Database;
use hearthsound_library::query::{self, PreferredFile};
use hearthsound_library::{LibraryError, MediaFile, StorageRoots, tracks};
use hearthsound_media::{FfmpegRunner, MediaError, TranscodeFormat, TranscodeRequest};
use tower_http::services::ServeFile;
use uuid::Uuid;

/// One `/rest/stream` request, after the endpoint read its parameters.
///
/// The endpoint owns the translation from Subsonic parameters to this type, so
/// the service never reads a query string. The method and the headers are part
/// of the request because serving a file is an HTTP conversation: `HEAD` asks
/// for the head alone, and `Range`, `If-None-Match`, `If-Modified-Since`, and
/// `If-Range` decide what body, if any, the answer carries.
#[derive(Debug, Clone)]
pub struct StreamRequest {
    /// The `id` parameter: the client id of the track to stream.
    pub id: String,
    /// The account the stream is served to.
    pub user_id: Uuid,
    /// The client that asked, when it named itself with `c`, for the stream
    /// session the activity guard reads.
    pub client: Option<String>,
    /// The request method, `GET` or `HEAD`.
    pub method: Method,
    /// The request's headers.
    pub headers: HeaderMap,
    /// What the `format` parameter asked for.
    pub format: StreamFormat,
    /// The `maxBitRate` parameter in kbit/s, when it asks for a cap. Zero, the
    /// protocol's "no limit", is read as no cap at all.
    pub max_bitrate_kbps: Option<u32>,
    /// The `timeOffset` parameter: how far into the track the stream starts.
    pub offset: Option<Duration>,
    /// How many channels a transcode carries, when the request caps them.
    pub max_channels: Option<u8>,
    /// Whether the requester may acquire a track the server has not stored.
    ///
    /// The endpoint passes the principal's `can_acquire` permission, because a
    /// pending stream starts an acquisition on the requester's behalf; the
    /// service answers `403` rather than searching for a user who may not.
    pub can_acquire: bool,
}

/// What the `format` parameter asked for.
///
/// A client either leaves the format to the server, asks for the stored file
/// untouched, or names a format to transcode to. The three are distinct because
/// `raw` overrides a bitrate cap, while an absent format does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    /// The parameter was absent or empty, so the server picks: the stored file
    /// unless a bitrate cap or an offset needs ffmpeg.
    Unspecified,
    /// `format=raw`: the stored file, never a transcode, whatever else the
    /// request carries.
    Raw,
    /// A format to transcode to.
    Transcode(TranscodeFormat),
}

/// Why a stream could not be served.
///
/// A stream never answers a Subsonic envelope, so each variant maps onto the
/// HTTP status [`StreamError::status`] returns.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The `id` is malformed, names something that is not a track, names a
    /// track the server does not know, or names a track that is cooling down
    /// after a failed acquisition and cannot be started again yet.
    #[error("the requested id does not name a stored track")]
    NotFound,
    /// The requester may not acquire the track this stream would need.
    #[error("the requester may not acquire a track the server has not stored")]
    Forbidden,
    /// The track is still being acquired and the wait elapsed without a file.
    ///
    /// The response carries a `Retry-After` header, because the acquisition
    /// continues and a client that waits may get the file on its next try.
    #[error("the track is still being acquired")]
    Unavailable {
        /// How many seconds the client should wait before retrying.
        retry_after_secs: u64,
    },
    /// The library could not be read.
    #[error(transparent)]
    Library(#[from] LibraryError),
    /// The track's file could not be read.
    #[error("the track's file could not be read")]
    File(#[source] std::io::Error),
    /// A response could not be built, which is a server-side failure.
    #[error("the stream could not be prepared")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl StreamError {
    /// Returns the HTTP status this failure is answered with.
    ///
    /// # Returns
    ///
    /// `404` for an id that names no track, a track the server has no file for,
    /// and a track that is cooling down; `403` for a requester who may not
    /// acquire; `503` for an acquisition that is still running without a file;
    /// and `500` for everything else. Neither case is a Subsonic envelope, and a
    /// client must be able to tell "there is no such track" from "the server
    /// just failed".
    pub fn status(&self) -> StatusCode {
        match self {
            StreamError::NotFound => StatusCode::NOT_FOUND,
            StreamError::Forbidden => StatusCode::FORBIDDEN,
            StreamError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            StreamError::File(source) if source.kind() == std::io::ErrorKind::NotFound => {
                StatusCode::NOT_FOUND
            }
            StreamError::Library(_) | StreamError::File(_) | StreamError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// Builds the response an endpoint answers with.
    ///
    /// The response has no body beyond the status line, and carries the headers
    /// a status alone cannot express: [`StreamError::Unavailable`] adds a
    /// `Retry-After`, which tells a client when the still-running acquisition
    /// may have produced the file.
    ///
    /// # Returns
    ///
    /// The empty response for this failure's [`status`](StreamError::status).
    pub fn response(&self) -> Response {
        let mut response = self.status().into_response();
        if let StreamError::Unavailable { retry_after_secs } = self
            && let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

/// Serves the bytes of one track to one client.
///
/// The service is shared the way every other service is: it holds handles, so
/// cloning it is cheap, and it keeps no per-request state.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use std::sync::Arc;
///
/// use hearthsound_activity::StreamSessions;
/// use hearthsound_config::settings::Settings;
/// use hearthsound_db::Database;
/// use hearthsound_library::StorageRoots;
/// use hearthsound_media::FfmpegRunner;
///
/// # async fn example(
/// #     database: Arc<Database>,
/// #     settings: Settings,
/// #     ffmpeg: Arc<FfmpegRunner>,
/// # ) {
/// let service = hearthsound_api_subsonic::StreamService::new(
///     database,
///     settings,
///     StorageRoots::new(Path::new("/srv/library"), Path::new("/srv/cache")),
///     ffmpeg,
///     StreamSessions::new(),
///     None,
/// );
/// # let _ = service;
/// # }
/// ```
#[derive(Clone)]
pub struct StreamService {
    /// The open database the file lookup and the access record use.
    database: Arc<Database>,
    /// The settings service the pending-stream mode and its wait bounds are read
    /// from, once per pending request.
    settings: Settings,
    /// The managed storage roots a file's path resolves against.
    roots: StorageRoots,
    /// The ffmpeg runner a transcode and a pending stream run through.
    ffmpeg: Arc<FfmpegRunner>,
    /// The stream sessions an active stream is registered in.
    sessions: StreamSessions,
    /// The acquisition layer a track without a file is requested from, when the
    /// composition root has one.
    pending: Option<Arc<dyn PendingSource>>,
}

impl std::fmt::Debug for StreamService {
    /// Writes the fields a debug reader needs, without the trait object.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamService")
            .field("database", &self.database)
            .field("roots", &self.roots)
            .field("sessions", &self.sessions)
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl StreamService {
    /// Builds the service.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database, for the preferred-file lookup and the
    ///   last-access record.
    /// - `settings`: the settings service, read for `stream.pending.mode` and
    ///   the `stream.hold`/`stream.pending` wait bounds when a request reaches a
    ///   track without a file.
    /// - `roots`: the managed storage roots a `library` or `cache` file
    ///   resolves against; a file inside a library folder resolves against the
    ///   folder's own path.
    /// - `ffmpeg`: the runner a transcode runs through, shared with the
    ///   composition root so one cap covers every transcode the process runs.
    /// - `sessions`: the registry of stream sessions, which must be the same
    ///   registry the activity service reads, or the "no song audio delivered"
    ///   guard cannot see this stream. The composition root takes it from
    ///   [`ActivityService::sessions`](hearthsound_activity::ActivityService::sessions).
    /// - `pending`: the acquisition layer a pending request is answered from,
    ///   or `None` when the composition root runs without one, in which case a
    ///   track without a file is answered `404` as before.
    pub fn new(
        database: Arc<Database>,
        settings: Settings,
        roots: StorageRoots,
        ffmpeg: Arc<FfmpegRunner>,
        sessions: StreamSessions,
        pending: Option<Arc<dyn PendingSource>>,
    ) -> StreamService {
        StreamService {
            database,
            settings,
            roots,
            ffmpeg,
            sessions,
            pending,
        }
    }

    /// Returns the managed storage roots.
    ///
    /// A media file that is not inside a library folder resolves against one of
    /// them, which is how a stored relative path becomes an absolute one.
    pub fn roots(&self) -> &StorageRoots {
        &self.roots
    }

    /// Sets the acquisition layer pending requests are answered from.
    ///
    /// # Parameters
    ///
    /// - `pending`: the acquisition layer, which the composition root builds
    ///   after the state, or `None` to answer a track without a file `404`.
    ///
    /// # Returns
    ///
    /// The service, sharing everything else with the service it was called on.
    pub fn with_pending_source(mut self, pending: Arc<dyn PendingSource>) -> StreamService {
        self.pending = Some(pending);
        self
    }

    /// Serves one request and returns the response to write.
    ///
    /// # Parameters
    ///
    /// - `request`: the request, whose `id` names the track to stream.
    ///
    /// # Returns
    ///
    /// The response: the file itself for a whole or partial request, an empty
    /// `304` for a conditional request that is up to date, or an empty `416`
    /// for a range past the end of the file.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NotFound`] for an id that names no stored track
    /// and for a track that cannot be acquired, [`StreamError::Forbidden`] for
    /// a requester who may not acquire, and a server-side error when the
    /// library, the file, or the acquisition layer cannot be read. The caller
    /// answers with [`StreamError::status`], never with a Subsonic envelope.
    pub async fn serve(&self, request: &StreamRequest) -> Result<Response, StreamError> {
        let Ok(id) = EntityId::parse(&request.id) else {
            tracing::debug!(id = %request.id, "a stream request named a malformed id");
            return Err(StreamError::NotFound);
        };
        if id.kind() != IdPrefix::Track {
            tracing::debug!(
                id = %id,
                "a stream request named an entity that is not a track"
            );
            return Err(StreamError::NotFound);
        }
        match self.stored(request, id).await? {
            Some(response) => Ok(response),
            None => self.pending(request, id).await,
        }
    }

    /// Serves the stored file of one track as a download.
    ///
    /// A download is not playback: the file is served with the range and
    /// validator handling a stream has, and no stream session is registered and
    /// no access is recorded, so the activity guard and the prefetch scheduler
    /// never see it. Only a stored track with a preferred file is served, because
    /// downloading a track the server does not have is not something a client can
    /// ask for.
    ///
    /// # Parameters
    ///
    /// - `request`: the request being served. Its format, bitrate cap, and
    ///   offset are ignored: a download is always the stored file whole.
    ///
    /// # Returns
    ///
    /// The file's bytes, with its validators and the range handling a client
    /// that resumes a download needs.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NotFound`] when the id names no stored track,
    /// when the track has no file, and when the file is missing from disk, and
    /// a server-side error when the library or the file cannot be read.
    pub async fn download(&self, request: &StreamRequest) -> Result<Response, StreamError> {
        let track = match EntityId::parse(&request.id) {
            Ok(id) if id.kind() == IdPrefix::Track => id,
            _ => {
                tracing::debug!(id = %request.id, "a download named no stored track");
                return Err(StreamError::NotFound);
            }
        };
        let file = {
            let mut connection = self
                .database
                .readers()
                .acquire()
                .await
                .map_err(LibraryError::from)?;
            query::preferred_media_file(&mut connection, &self.roots, track).await?
        };
        let Some(file) = file else {
            tracing::debug!(track = %track, "a download named a track with no file");
            return Err(StreamError::NotFound);
        };
        self.serve_file(request, track, &file, false).await
    }

    /// Serves the track's stored file when it has one.
    ///
    /// The lookup connection is released before the file is served, so a long
    /// stream never holds a reader connection.
    ///
    /// # Parameters
    ///
    /// - `request`: the request being served.
    /// - `track`: the stored track to look a file up for.
    ///
    /// # Returns
    ///
    /// `Some(response)` when the track has a preferred file, `None` when the
    /// track is stored but has no file yet.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NotFound`] when no track has that id, and a
    /// server-side error when the library or the file cannot be read.
    async fn stored(
        &self,
        request: &StreamRequest,
        track: EntityId,
    ) -> Result<Option<Response>, StreamError> {
        let (file, known) = {
            let mut connection = self
                .database
                .readers()
                .acquire()
                .await
                .map_err(LibraryError::from)?;
            let file = query::preferred_media_file(&mut connection, &self.roots, track).await?;
            let known = if file.is_none() {
                tracks::get_track(&mut connection, track).await?.is_some()
            } else {
                true
            };
            (file, known)
        };

        match file {
            Some(file) => {
                let response = match delivery(request, &file.media_file) {
                    Delivery::File => self.serve_file(request, track, &file, true).await?,
                    Delivery::Transcode(plan) => {
                        self.serve_transcode(request, track, &file, &plan).await?
                    }
                };
                Ok(Some(response))
            }
            None if known => Ok(None),
            None => {
                tracing::debug!(track = %track, "a stream request named an unknown track");
                Err(StreamError::NotFound)
            }
        }
    }

    /// Answers a request for a track the server has no file to serve yet.
    ///
    /// The track is requested from the acquisition layer at priority
    /// [`Priority::Play`]. What the acquisition answers decides the response:
    ///
    /// - an available track is served by [`StreamService::stored`], so a
    ///   request that raced an import still gets its bytes;
    /// - a cooling-down track is `404`: its last acquisition failed and no new
    ///   search may start until the cooldown elapses, so the request starts
    ///   nothing;
    /// - a denied request is `403`;
    /// - a started or joined acquisition is answered by
    ///   [`StreamService::serve_pending`].
    ///
    /// # Parameters
    ///
    /// - `request`: the request that could not be resolved to a stored file.
    /// - `track`: the track row the request named.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NotFound`] when there is no acquisition layer and
    /// when the track cannot be acquired now, and [`StreamError::Forbidden`] when
    /// the requester may not acquire.
    async fn pending(
        &self,
        request: &StreamRequest,
        track: EntityId,
    ) -> Result<Response, StreamError> {
        let Some(source) = &self.pending else {
            tracing::debug!(
                id = %request.id,
                "no stored file to serve for the requested track and no acquisition layer"
            );
            return Err(StreamError::NotFound);
        };

        // The permission is checked before anything is requested: an account that
        // may not acquire must not start one.
        if !request.can_acquire {
            tracing::debug!(
                id = %request.id,
                user = %request.user_id,
                "refused a pending stream for a user who may not acquire"
            );
            return Err(StreamError::Forbidden);
        }

        let outcome = source
            .request(
                track,
                Priority::Play,
                Some(request.user_id),
                request.can_acquire,
            )
            .await
            .map_err(internal)?;

        match outcome {
            PendingOutcome::Available => match self.stored(request, track).await? {
                Some(response) => Ok(response),
                None => {
                    tracing::debug!(track = %track, "an available track still has no file to serve");
                    Err(StreamError::NotFound)
                }
            },
            PendingOutcome::CoolingDown => {
                tracing::debug!(
                    track = %track,
                    "refused a pending stream while the track cools down"
                );
                Err(StreamError::NotFound)
            }
            PendingOutcome::Denied { reason } => {
                tracing::debug!(
                    track = %track,
                    reason = reason.as_str(),
                    "the acquisition layer denied a pending stream"
                );
                Err(StreamError::Forbidden)
            }
            PendingOutcome::Pending(handle) => self.serve_pending(request, track, handle).await,
        }
    }

    /// Answers a request whose acquisition was started or joined.
    ///
    /// The mode comes from `stream.pending.mode`: `hold`, the default, waits for
    /// the file and serves it as a stored track would be, and `fail` answers
    /// `503` at once while the acquisition continues in the background.
    ///
    /// An acquisition parked for a source is answered `503` with `Retry-After`
    /// whatever the mode says: it waits for a plugin instance to be added, which
    /// `stream.pending.max_wait_secs` has no reason to cover, and holding the
    /// client's connection open for that would look like a stalled server.
    ///
    /// # Parameters
    ///
    /// - `request`: the request being answered.
    /// - `track`: the track being acquired.
    /// - `handle`: the started or joined acquisition to watch.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Unavailable`] for a parked acquisition and for a
    /// `fail` mode answer, and whatever [`StreamService::serve_held`] returns.
    async fn serve_pending(
        &self,
        request: &StreamRequest,
        track: EntityId,
        handle: AcquisitionHandle,
    ) -> Result<Response, StreamError> {
        if handle.snapshot().awaiting_source {
            tracing::debug!(
                track = %track,
                "answered a pending stream with 503 while its acquisition waits for a source"
            );
            return Err(StreamError::Unavailable {
                retry_after_secs: RETRY_AFTER_SECS,
            });
        }

        let settings = self.settings.current();
        let mode = PendingMode::of(&setting_or(
            &settings,
            keys::STREAM_PENDING_MODE,
            "hold".to_owned(),
        ));
        match mode {
            PendingMode::Hold => self.serve_held(request, track, handle).await,
            PendingMode::Fail => {
                tracing::debug!(
                    track = %track,
                    "answered a pending stream with 503 while its acquisition runs"
                );
                Err(StreamError::Unavailable {
                    retry_after_secs: RETRY_AFTER_SECS,
                })
            }
        }
    }

    /// Waits for a pending acquisition's file and serves it.
    ///
    /// Nothing is sent until the file is ready, so no headers are written while
    /// the acquisition runs and a failure, a cooldown, or an elapsed wait can
    /// still be answered as a status code. The wait is bounded by
    /// `stream.pending.max_wait_secs`, after which the request is answered `503`
    /// with `Retry-After` because the acquisition continues.
    ///
    /// An acquisition that parks for a source while the request is held ends
    /// the hold the same way a park before it began does:
    /// [`AcquisitionHandle::wait_until_settled`] returns the parked snapshot, so
    /// a backend that stops answering is answered `503` at once instead of
    /// leaving the client's connection open for the rest of the wait.
    ///
    /// # Parameters
    ///
    /// - `request`: the request being answered; a completed acquisition is
    ///   served exactly as a stored track would be, raw or transcoded.
    /// - `track`: the track being acquired.
    /// - `handle`: the acquisition to watch.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NotFound`] when the acquisition ended without a
    /// file, and [`StreamError::Unavailable`] when it parked for a source or the
    /// wait elapsed first.
    async fn serve_held(
        &self,
        request: &StreamRequest,
        track: EntityId,
        handle: AcquisitionHandle,
    ) -> Result<Response, StreamError> {
        let max_wait = pending_max_wait(&self.settings.current());
        match tokio::time::timeout(max_wait, handle.wait_until_settled()).await {
            Ok(Ok(progress)) if progress.awaiting_source => {
                tracing::debug!(
                    track = %track,
                    acquisition = %progress.acquisition_id,
                    park_reason = progress.park_reason.map(|reason| reason.as_str()),
                    "a held stream's acquisition parked for a source"
                );
                Err(StreamError::Unavailable {
                    retry_after_secs: RETRY_AFTER_SECS,
                })
            }
            Ok(outcome) => self.serve_held_outcome(request, track, outcome).await,
            Err(_) => {
                tracing::debug!(
                    track = %track,
                    waited = ?max_wait,
                    "a held stream waited out its acquisition"
                );
                Err(StreamError::Unavailable {
                    retry_after_secs: RETRY_AFTER_SECS,
                })
            }
        }
    }

    /// Serves the file of a watched acquisition that just ended.
    ///
    /// # Parameters
    ///
    /// - `request`: the request being answered.
    /// - `track`: the track that was acquired.
    /// - `outcome`: the acquisition's terminal snapshot, or its error when the
    ///   progress channel closed, which means the server is stopping.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NotFound`] when the acquisition failed, was
    /// cancelled, or completed without a servable file.
    async fn serve_held_outcome(
        &self,
        request: &StreamRequest,
        track: EntityId,
        outcome: Result<AcquisitionProgress, AcquireError>,
    ) -> Result<Response, StreamError> {
        match outcome {
            Ok(progress) if progress.succeeded() => match self.stored(request, track).await? {
                Some(response) => Ok(response),
                None => {
                    tracing::warn!(
                        track = %track,
                        "an acquisition completed but the track has no file to serve"
                    );
                    Err(StreamError::NotFound)
                }
            },
            Ok(progress) => {
                tracing::debug!(
                    acquisition = %progress.acquisition_id,
                    state = progress.state.as_str(),
                    "a held stream's acquisition ended without a file"
                );
                Err(StreamError::NotFound)
            }
            Err(error) => {
                tracing::debug!(%error, "a held stream lost its acquisition");
                Err(StreamError::NotFound)
            }
        }
    }

    /// Serves the track's preferred file.
    ///
    /// # Parameters
    ///
    /// - `request`: the request, whose method and conditional headers decide
    ///   the answer.
    /// - `track`: the track the file belongs to, for the stream session.
    /// - `file`: the media file row and the absolute path it resolves to.
    /// - `session`: whether the delivery is a stream, which registers the
    ///   session the activity guard reads; a download passes `false`.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::NotFound`] when the file no longer exists and a
    /// server-side error when it cannot be read.
    async fn serve_file(
        &self,
        request: &StreamRequest,
        track: EntityId,
        file: &PreferredFile,
        session: bool,
    ) -> Result<Response, StreamError> {
        let path = file.absolute_path.as_path();
        let metadata = match tokio::fs::metadata(path).await {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    file = %path.display(),
                    track = %track,
                    "the preferred file of a track is missing from disk"
                );
                return Err(StreamError::NotFound);
            }
            Err(source) => return Err(StreamError::File(source)),
        };
        let tags = EntityTags::of(&file.media_file, &metadata)?;

        if let Some(response) = tags.revalidate(request)? {
            return Ok(response);
        }

        let mut upstream = Request::new(Body::empty());
        *upstream.method_mut() = request.method.clone();
        if let Some(range) = tags.fresh_range(request) {
            upstream.headers_mut().insert(header::RANGE, range);
        }
        let served = ServeFile::new(path)
            .try_call(upstream)
            .await
            .map_err(StreamError::File)?;
        let (mut parts, body) = served.into_parts();

        parts.headers.insert(header::ETAG, tags.etag.clone());
        if let Some(last_modified) = &tags.last_modified {
            parts
                .headers
                .insert(header::LAST_MODIFIED, last_modified.clone());
        }

        // Only a response that carries the file or a part of it is a stream the
        // user hears: a `HEAD`, a refused range, and a revalidation send no
        // audio, so none of them registers a session or records an access.
        let streaming = session
            && matches!(parts.status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
            && request.method != Method::HEAD;

        // The container or codec decides the content type, not the file name
        // alone; a suffix the library does not know keeps the type the file
        // service guessed.
        let content_type = query::content_type_for(&file.media_file.suffix());
        if content_type != "application/octet-stream" && parts.status.is_success() {
            parts
                .headers
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        }

        let body = if streaming {
            let (delivered, flag) = AudioDelivered::flag();
            self.sessions.register(
                request.user_id,
                track.uuid(),
                request.client.as_deref(),
                delivered,
            );
            self.record_access(file.media_file.id);
            delivered_body(Body::new(body).into_data_stream(), flag)
        } else {
            Body::new(body)
        };
        Ok(Response::from_parts(parts, body))
    }

    /// Serves the track's file through ffmpeg.
    ///
    /// A transcode is one ffmpeg process per request and its output does not
    /// exist until that process runs, so the response has no validators and no
    /// ranges: the client is told `Accept-Ranges: none` and receives the whole
    /// stream from the start offset on. The body is chunked and carries no
    /// `Content-Length`, because the encoder's byte count is only known when it
    /// exits and HTTP requires a declared length to be exact; an
    /// `estimateContentLength=true` request is accepted and changes nothing.
    ///
    /// The status is decided before the response is committed: the first encoded
    /// chunk is awaited, so an encoder that cannot start is answered `500`
    /// instead of a `200` whose body is an error.
    ///
    /// A `HEAD` request is answered from the request alone: the head of a
    /// transcode is known without encoding anything, so no ffmpeg process is
    /// started for it.
    ///
    /// # Parameters
    ///
    /// - `request`: the request, whose method and account the answer needs.
    /// - `track`: the track being streamed, for the stream session.
    /// - `file`: the media file row and the path ffmpeg reads.
    /// - `plan`: the format, the bitrate cap, and the start offset.
    ///
    /// # Errors
    ///
    /// Returns a server-side error when ffmpeg cannot be started, when it fails
    /// before its first chunk, or when a header cannot be built.
    async fn serve_transcode(
        &self,
        request: &StreamRequest,
        track: EntityId,
        file: &PreferredFile,
        plan: &TranscodePlan,
    ) -> Result<Response, StreamError> {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::OK;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(plan.format.content_type()),
        );
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
        if request.method == Method::HEAD {
            return Ok(response);
        }

        let mut transcode = TranscodeRequest::new(&file.absolute_path, plan.format);
        if plan.max_bitrate_kbps.is_some() {
            transcode = transcode.max_bitrate_kbps(plan.max_bitrate_kbps);
        }
        if let Some(offset) = plan.offset {
            transcode = transcode.offset(offset);
        }
        if plan.max_channels.is_some() {
            transcode = transcode.max_channels(plan.max_channels);
        }
        let mut encoded = Box::pin(self.ffmpeg.transcode(&transcode).await.map_err(internal)?);
        // The status is committed to 200 as soon as the body is handed over, so
        // the first chunk is awaited first: an encoder that fails to start then
        // answers 500 instead of a 200 whose body is an error. A slower encoder
        // simply streams into the committed response.
        let first = first_chunk(tokio::time::timeout(FIRST_CHUNK_TIMEOUT, encoded.next()).await)
            .map_err(internal)?;

        let (delivered, flag) = AudioDelivered::flag();
        self.sessions.register(
            request.user_id,
            track.uuid(),
            request.client.as_deref(),
            delivered,
        );
        self.record_access(file.media_file.id);
        *response.body_mut() = delivered_body(stream::iter(first.map(Ok)).chain(encoded), flag);
        Ok(response)
    }

    /// Records that the file was just streamed.
    ///
    /// The write is detached: it runs on its own task against the writer pool,
    /// so it never delays the first byte of the stream, and a failure is logged
    /// and dropped because the record only feeds the retention policy.
    ///
    /// # Parameters
    ///
    /// - `media_file_id`: the file that was served.
    fn record_access(&self, media_file_id: Uuid) {
        let database = Arc::clone(&self.database);
        tokio::spawn(async move {
            let mut connection = match database.writer().acquire().await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(%error, "could not record that a media file was streamed");
                    return;
                }
            };
            if let Err(error) = query::touch_last_accessed(&mut connection, media_file_id).await {
                tracing::warn!(%error, "could not record that a media file was streamed");
            }
        });
    }
}

/// The acquisition layer a pending stream asks for a track it has not stored.
///
/// [`StreamService`] depends on this narrow seam rather than on the acquisition
/// orchestrator directly, so its pending-stream logic is testable with a source
/// that reports a scripted progress snapshot while production wires
/// [`AcquisitionOrchestrator`], whose implementation is below.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
/// use hearthsound_api_subsonic::PendingSource;
///
/// # fn example(orchestrator: Arc<AcquisitionOrchestrator>) {
/// let source: Arc<dyn PendingSource> = orchestrator;
/// # let _ = source;
/// # }
/// ```
#[async_trait]
pub trait PendingSource: Send + Sync {
    /// Asks the acquisition layer for one track.
    ///
    /// # Parameters
    ///
    /// - `track`: the track to acquire.
    /// - `priority`: how urgent the acquisition is; a stream asks at
    ///   [`Priority::Play`].
    /// - `requested_by`: the user whose request created the acquisition.
    /// - `can_acquire`: whether that user may acquire at all.
    ///
    /// # Returns
    ///
    /// What admission decided: the track is available, an acquisition to watch,
    /// a cooldown, or a denial.
    ///
    /// # Errors
    ///
    /// Returns the acquisition layer's error when admission itself failed.
    async fn request(
        &self,
        track: EntityId,
        priority: Priority,
        requested_by: Option<Uuid>,
        can_acquire: bool,
    ) -> Result<PendingOutcome, AcquireError>;
}

/// What the acquisition layer decided about a pending track.
#[derive(Debug)]
pub enum PendingOutcome {
    /// The track already has a file, so the stream service serves it.
    Available,
    /// An acquisition was started or joined, and can be watched to its end.
    Pending(AcquisitionHandle),
    /// The track failed recently and cannot be retried until its cooldown ends.
    CoolingDown,
    /// The acquisition layer refused the request.
    ///
    /// The reason says whether the requester lacks the `can_acquire` permission
    /// or has spent its daily acquisition quota; a stream answers `403` for
    /// both, because either way it cannot hand the client a file.
    Denied {
        /// Why the request was refused.
        reason: FailureReason,
    },
}

#[async_trait]
impl PendingSource for AcquisitionOrchestrator {
    /// Admits or joins a `play` acquisition for a stream request.
    async fn request(
        &self,
        track: EntityId,
        priority: Priority,
        requested_by: Option<Uuid>,
        can_acquire: bool,
    ) -> Result<PendingOutcome, AcquireError> {
        let outcome = AcquisitionOrchestrator::request(
            self,
            AcquireRequest::new(track, priority, "stream")
                .with_requested_by(requested_by)
                .with_can_acquire(can_acquire),
        )
        .await?;
        Ok(match outcome {
            AcquireOutcome::AlreadyAvailable { .. } => PendingOutcome::Available,
            AcquireOutcome::Joined(handle) | AcquireOutcome::Started(handle) => {
                PendingOutcome::Pending(handle)
            }
            AcquireOutcome::CoolingDown { .. } => PendingOutcome::CoolingDown,
            AcquireOutcome::Denied { reason, .. } => PendingOutcome::Denied { reason },
        })
    }
}

/// How long a client is told to wait after a `503`.
const RETRY_AFTER_SECS: u64 = 10;

/// How long the first encoded chunk may take before the response commits to
/// `200`; a slower encoder streams into the committed response.
const FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(5);

/// Decides what a transcode's first awaited chunk says about the status.
///
/// The status cannot change once the body is handed over, so the first chunk is
/// awaited before the response is committed: an encoder that failed before it
/// produced anything is answered as the failure it is, while one that is merely
/// slow - no chunk within [`FIRST_CHUNK_TIMEOUT`], or none at all - streams into
/// the committed `200`.
///
/// # Parameters
///
/// - `awaited`: the first chunk under the bound, or the elapsed timeout.
///
/// # Returns
///
/// The chunk to yield before the rest of the stream, when there is one.
///
/// # Errors
///
/// Returns the encoder's own error, which is answered `500`.
fn first_chunk(
    awaited: Result<Option<Result<Bytes, MediaError>>, tokio::time::error::Elapsed>,
) -> Result<Option<Bytes>, MediaError> {
    match awaited {
        Ok(Some(Err(error))) => Err(error),
        Ok(Some(Ok(chunk))) => Ok(Some(chunk)),
        Ok(None) | Err(_) => Ok(None),
    }
}

/// The pending-stream mode `stream.pending.mode` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingMode {
    /// Send nothing until the file is ready, then serve it normally.
    Hold,
    /// Answer `503` with `Retry-After` at once.
    Fail,
}

impl PendingMode {
    /// Reads a `stream.pending.mode` literal.
    ///
    /// # Parameters
    ///
    /// - `value`: the setting's current value.
    ///
    /// # Returns
    ///
    /// The named mode, or [`PendingMode::Hold`] for an absent or unknown
    /// literal, which is the documented default.
    fn of(value: &str) -> PendingMode {
        match value {
            "fail" => PendingMode::Fail,
            _ => PendingMode::Hold,
        }
    }
}

/// Reads a setting a pending request needs, falling back to its default.
///
/// A settings read that fails is logged and stays with the built-in default: a
/// stream should not fail because a tunable is temporarily unreadable.
///
/// # Parameters
///
/// - `store`: the settings snapshot the value is read from.
/// - `key`: the setting to read.
/// - `fallback`: the value to use when the read fails.
///
/// # Returns
///
/// The stored value, or `fallback`.
fn setting_or<T>(store: &SettingsStore, key: SettingKey<T>, fallback: T) -> T
where
    T: SettingType + Clone,
{
    match store.get(key) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                key = key.name(),
                %error,
                "could not read a pending-stream setting; using its default"
            );
            fallback
        }
    }
}

/// Returns how long a pending stream may play before it ends without a file.
///
/// # Parameters
///
/// - `store`: the settings snapshot to read `stream.pending.max_wait_secs` from.
///
/// # Returns
///
/// The configured wait, or 180 seconds for a negative one.
fn pending_max_wait(store: &SettingsStore) -> Duration {
    let secs = setting_or(store, keys::STREAM_PENDING_MAX_WAIT_SECS, 180);
    Duration::from_secs(u64::try_from(secs).unwrap_or(180))
}

/// Wraps an encoded stream so that its first chunk marks the stream as audibly
/// delivered.
///
/// # Parameters
///
/// - `stream`: the encoded bytes, whether the file service produced them or an
///   ffmpeg process did.
/// - `flag`: the flag the registered stream session reads.
///
/// # Returns
///
/// The axum body the response carries.
fn delivered_body<S, E>(stream: S, flag: Arc<AtomicBool>) -> Body
where
    S: TryStream<Ok = Bytes, Error = E> + Send + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
    Body::from_stream(stream.inspect_ok(move |_| {
        flag.store(true, Ordering::SeqCst);
    }))
}

/// How one request is delivered.
enum Delivery {
    /// The stored file, byte for byte.
    File,
    /// A transcode of the stored file.
    Transcode(TranscodePlan),
}

/// The transcode a request asked for.
struct TranscodePlan {
    /// The format to encode.
    format: TranscodeFormat,
    /// The bitrate cap in kbit/s, or `None` for the format's default.
    max_bitrate_kbps: Option<u32>,
    /// How far into the track the stream starts.
    offset: Option<Duration>,
    /// How many channels the output carries, or `None` for the source's own.
    max_channels: Option<u8>,
}

/// Decides whether a request is served from the file or through ffmpeg.
///
/// The stored file answers a request when it already is what the client asked
/// for: no format and no cap, an explicit `format=raw`, a bitrate cap the file
/// already fits, or the requested format with no offset. Everything else - a
/// different format, a cap below the file's own bitrate, or an offset into the
/// track - is transcoded. A request that names only a cap is transcoded to mp3,
/// which is what the clients of this protocol expect of a server that caps the
/// bitrate.
///
/// `format=raw` is answered from the stored file byte for byte, so `timeOffset`
/// and `maxBitRate` are ignored with it: an offset can only be honoured by
/// transcoding, which is no longer the stored bytes. A client that wants either
/// omits `format`.
///
/// # Parameters
///
/// - `request`: the request's format, cap, and offset.
/// - `file`: the media file the track resolved to.
///
/// # Returns
///
/// The delivery to serve.
fn delivery(request: &StreamRequest, file: &MediaFile) -> Delivery {
    match request.format {
        // `format=raw` asks for the stored bytes; a cap alongside it cannot be
        // honored without transcoding, and serving the file is what the format
        // asks for.
        StreamFormat::Raw => Delivery::File,
        StreamFormat::Transcode(format) => {
            let already = is_encoded_as(format, file)
                && !cap_below_file(request.max_bitrate_kbps, file)
                && request.offset.is_none();
            if already {
                Delivery::File
            } else {
                Delivery::Transcode(plan(format, request))
            }
        }
        StreamFormat::Unspecified => {
            if cap_below_file(request.max_bitrate_kbps, file) || request.offset.is_some() {
                Delivery::Transcode(plan(TranscodeFormat::Mp3, request))
            } else {
                Delivery::File
            }
        }
    }
}

/// Reports whether the file is already encoded in the format's codec.
///
/// # Parameters
///
/// - `format`: the format the client asked for.
/// - `file`: the media file the track resolved to.
///
/// # Returns
///
/// `true` when the probe recorded the format's codec literal for the file.
fn is_encoded_as(format: TranscodeFormat, file: &MediaFile) -> bool {
    file.codec
        .as_deref()
        .is_some_and(|codec| codec.eq_ignore_ascii_case(format.as_str()))
}

/// Reports whether a bitrate cap is below what the file already is.
///
/// # Parameters
///
/// - `max_bitrate_kbps`: the request's cap, or `None` for no cap.
/// - `file`: the media file the track resolved to.
///
/// # Returns
///
/// `true` when the cap is below the file's bitrate, and when the file's bitrate
/// is unknown, because a file that cannot be shown to fit the cap is
/// transcoded.
fn cap_below_file(max_bitrate_kbps: Option<u32>, file: &MediaFile) -> bool {
    let Some(cap) = max_bitrate_kbps else {
        return false;
    };
    match file.bitrate_kbps.and_then(|kbps| u32::try_from(kbps).ok()) {
        Some(bitrate) => cap < bitrate,
        None => true,
    }
}

/// Builds the transcode plan of a request.
///
/// # Parameters
///
/// - `format`: the format to encode.
/// - `request`: the request supplying the cap and the offset.
///
/// # Returns
///
/// The plan to run.
fn plan(format: TranscodeFormat, request: &StreamRequest) -> TranscodePlan {
    TranscodePlan {
        format,
        max_bitrate_kbps: request.max_bitrate_kbps,
        offset: request.offset,
        max_channels: request.max_channels,
    }
}

/// Estimates the length of a transcode.
///
/// The estimate is the bitrate the encoder was asked for multiplied by the
/// remaining duration, which is what `estimateContentLength` asks for; the
/// exact length is only known once the encoder has finished.
///
/// The validators of one file response.
///
/// The `ETag` is strong and derived from the file's size, its modification time
/// and the media file's id, so it changes when the file is replaced, when it is
/// re-encoded in place, and when a track's preferred file becomes another row
/// that happens to have the same size and timestamp.
struct EntityTags {
    /// The strong entity tag of the representation.
    etag: HeaderValue,
    /// The `Last-Modified` header, when the filesystem reported a time.
    last_modified: Option<HeaderValue>,
    /// The modification time the conditional requests compare against.
    modified: Option<SystemTime>,
}

impl EntityTags {
    /// Derives the validators of a file.
    ///
    /// # Parameters
    ///
    /// - `media_file`: the media file row being served.
    /// - `metadata`: the file's metadata, as the filesystem reports it now.
    ///
    /// # Returns
    ///
    /// The strong entity tag, and the modification time both header forms use.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Internal`] when a header value is rejected, which
    /// only happens for a byte sequence the header grammar forbids; the values
    /// built here are hexadecimal digits, a hyphen, a UUID, and an HTTP date.
    fn of(media_file: &MediaFile, metadata: &std::fs::Metadata) -> Result<EntityTags, StreamError> {
        let modified = metadata.modified().ok();
        let millis = modified
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_millis());
        let etag = HeaderValue::from_str(&format!(
            "\"{:x}-{:x}-{}\"",
            millis.unwrap_or(0),
            metadata.len(),
            media_file.id
        ))
        .map_err(internal)?;
        let last_modified = match modified {
            Some(time) => {
                Some(HeaderValue::from_str(&httpdate::fmt_http_date(time)).map_err(internal)?)
            }
            None => None,
        };
        Ok(EntityTags {
            etag,
            last_modified,
            modified,
        })
    }

    /// Answers a conditional request that does not need the body.
    ///
    /// `If-None-Match` is evaluated first and, when it is present, wins over
    /// `If-Modified-Since`, as RFC 9110 requires. A matching `If-None-Match`
    /// answers `304` for both `GET` and `HEAD`; an absent or stale validator
    /// lets the request continue.
    ///
    /// # Parameters
    ///
    /// - `request`: the request whose conditional headers are read.
    ///
    /// # Returns
    ///
    /// The `304` response when the client's copy is current, otherwise `None`.
    ///
    /// # Errors
    ///
    /// Returns [`StreamError::Internal`] when a header value cannot be built.
    fn revalidate(&self, request: &StreamRequest) -> Result<Option<Response>, StreamError> {
        let current = match request.headers.get(header::IF_NONE_MATCH) {
            Some(if_none_match) => if_none_match_matches(if_none_match, &self.etag),
            None => match (
                request.headers.get(header::IF_MODIFIED_SINCE),
                self.modified,
            ) {
                (Some(value), Some(modified)) => value
                    .to_str()
                    .ok()
                    .and_then(|value| httpdate::parse_http_date(value).ok())
                    .is_some_and(|since| truncate(modified) <= since),
                _ => false,
            },
        };
        if !current {
            return Ok(None);
        }
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        let headers = response.headers_mut();
        headers.insert(header::ETAG, self.etag.clone());
        if let Some(last_modified) = &self.last_modified {
            headers.insert(header::LAST_MODIFIED, last_modified.clone());
        }
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        Ok(Some(response))
    }

    /// Returns the `Range` header to serve, if the request may serve one.
    ///
    /// `If-Range` decides whether a range request still concerns the file the
    /// client saw: an entity tag that does not match, or a date the file has
    /// moved past, means the client's partial copy is from another
    /// representation, so the whole file is served instead.
    ///
    /// # Parameters
    ///
    /// - `request`: the request whose `Range` and `If-Range` headers are read.
    ///
    /// # Returns
    ///
    /// The `Range` header to pass on, or `None` to serve the whole file.
    fn fresh_range(&self, request: &StreamRequest) -> Option<HeaderValue> {
        let range = request.headers.get(header::RANGE)?;
        let Some(if_range) = request.headers.get(header::IF_RANGE) else {
            return Some(range.clone());
        };
        let Ok(if_range) = if_range.to_str() else {
            return None;
        };
        let fresh = if if_range.starts_with('"') || if_range.starts_with("W/") {
            // An entity tag is compared strongly: a weak tag never validates a
            // range.
            !if_range.starts_with("W/") && if_range == header_text(&self.etag)
        } else {
            match (httpdate::parse_http_date(if_range), self.modified) {
                (Ok(since), Some(modified)) => truncate(modified) <= since,
                _ => false,
            }
        };
        fresh.then(|| range.clone())
    }
}

/// Reports whether an `If-None-Match` header covers the current representation.
///
/// The comparison is weak, as RFC 9110 requires for `If-None-Match`: a `W/`
/// prefix on either side is ignored, `*` matches any existing representation,
/// and the list may hold several tags.
///
/// # Parameters
///
/// - `value`: the request's `If-None-Match` header.
/// - `etag`: the current representation's entity tag.
///
/// # Returns
///
/// `true` when the client's copy is current.
fn if_none_match_matches(value: &HeaderValue, etag: &HeaderValue) -> bool {
    let Ok(value) = value.to_str() else {
        return false;
    };
    let ours = header_text(etag);
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == ours
    })
}

/// Returns a header value as text without its weak marker.
///
/// # Parameters
///
/// - `value`: the header value to read.
///
/// # Returns
///
/// The value without a leading `W/`, or the empty string when it is not valid
/// ASCII text; the tags built by this module always are.
fn header_text(value: &HeaderValue) -> &str {
    let text = value.to_str().unwrap_or_default();
    text.strip_prefix("W/").unwrap_or(text)
}

/// Drops the subsecond part of an instant, as HTTP dates have second
/// resolution.
///
/// # Parameters
///
/// - `time`: the instant to truncate.
///
/// # Returns
///
/// The instant without its subsecond part, or the instant itself when it is
/// before the epoch.
fn truncate(time: SystemTime) -> SystemTime {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => UNIX_EPOCH + std::time::Duration::from_secs(since.as_secs()),
        Err(_) => time,
    }
}

/// Wraps a non-`io` failure as a server-side stream error.
///
/// # Parameters
///
/// - `error`: the failure to wrap.
///
/// # Returns
///
/// The error as [`StreamError::Internal`].
fn internal(error: impl std::error::Error + Send + Sync + 'static) -> StreamError {
    StreamError::Internal(Box::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a header value for a test.
    fn value(text: &str) -> HeaderValue {
        HeaderValue::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn a_transcode_commits_to_200_only_when_its_first_chunk_is_not_a_failure() {
        let chunk = Bytes::from_static(b"encoded");
        let first = first_chunk(Ok(Some(Ok(chunk.clone())))).unwrap();
        assert_eq!(
            first,
            Some(chunk),
            "the first chunk is yielded before the rest of the stream"
        );
        assert!(
            first_chunk(Ok(Some(Err(MediaError::Stream {
                message: "decode failed".to_owned(),
            }))))
            .is_err(),
            "an encoder that fails before its first chunk is answered 500"
        );
        assert!(
            first_chunk(Ok(None)).unwrap().is_none(),
            "an encoder that ended without a chunk streams into the committed 200"
        );

        // The elapsed bound is the same answer: the response commits and the
        // encoder keeps streaming into it.
        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<u8>())
            .await
            .unwrap_err();
        assert!(
            first_chunk(Err(elapsed)).unwrap().is_none(),
            "a slow encoder commits to 200 and streams into it"
        );
    }

    #[test]
    fn if_none_match_covers_a_weak_and_a_strong_tag() {
        let etag = value("\"abc\"");
        assert!(if_none_match_matches(&value("\"abc\""), &etag));
        assert!(if_none_match_matches(&value("W/\"abc\""), &etag));
        assert!(if_none_match_matches(&value("*"), &etag));
        assert!(if_none_match_matches(&value("\"other\", W/\"abc\""), &etag));
        assert!(!if_none_match_matches(&value("\"other\""), &etag));
    }

    #[test]
    fn a_weak_current_tag_still_covers_the_representation() {
        let etag = value("W/\"abc\"");
        assert!(if_none_match_matches(&value("\"abc\""), &etag));
        assert_eq!(header_text(&etag), "\"abc\"");
    }
}
