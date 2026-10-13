//! The stream service: the bytes behind `/rest/stream`.
//!
//! [`StreamService`] turns one [`StreamRequest`] into the HTTP response a
//! Subsonic client plays. A request for a stored track resolves to the track's
//! preferred media file, and that file is served straight from disk with the
//! HTTP semantics a seeking client needs: `Accept-Ranges`, `Range` and
//! `Content-Range`, `ETag` and `Last-Modified`, `If-None-Match`,
//! `If-Modified-Since`, `If-Range`, `HEAD`, and the codec's content type.
//!
//! The bytes themselves come from tower-http's `ServeFile`, which opens the
//! file, answers a single range with `206`, answers an unsatisfiable range with
//! `416`, and never materializes the whole file in memory. Everything that is
//! not a plain range - the validators and the conditional requests built on
//! them - is decided here, because the `ETag` is derived from the stored media
//! file (its size, its modification time, and its id) rather than from the path
//! alone, so a client that replaces a file revalidates instead of playing a
//! stale cache entry.
//!
//! # A stream is a session
//!
//! Serving bytes registers a stream session in the activity service's
//! [`StreamSessions`] registry, with the audio-delivered flag set the first
//! time a body frame is written to the client. That flag is what lets a later
//! `scrobble` refuse to record a play for a stream the client never heard (see
//! `hearthsound_activity::ActivityService::scrobble`), so it is set by the body
//! rather than when the response is built. Recording the file's last access is
//! a detached write: it never delays the first byte, and a failure is logged
//! and dropped, because it is bookkeeping for the retention policy rather than
//! part of the response.
//!
//! # Failures are status codes
//!
//! A stream never answers a Subsonic envelope (invariant 6 of the repository
//! rules): a client that treats any 200 body as audio would play an error
//! document as noise. [`StreamError`] therefore carries the HTTP status to
//! answer with - `404` for an id that names no track, `404` for a track the
//! server has no file for yet, and `500` for a file or a database the server
//! could not read.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, BodyDataStream};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;
use hearthsound_activity::{AudioDelivered, StreamSessions};
use hearthsound_core::{ClientId, EntityId, IdPrefix};
use hearthsound_db::Database;
use hearthsound_library::query::{self, PreferredFile};
use hearthsound_library::{LibraryError, MediaFile, StorageRoots, tracks};
use tower_http::services::ServeFile;
use tower_http::services::fs::ServeFileSystemResponseBody;
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
}

/// Why a stream could not be served.
///
/// A stream never answers a Subsonic envelope, so each variant maps onto the
/// HTTP status [`StreamError::status`] returns.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The `id` is malformed, names something that is not a track, or names a
    /// track the server does not know.
    #[error("the requested id does not name a stored track")]
    NotFound,
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
    /// `404` for an id that names no track and for a file that is gone, and
    /// `500` for everything else: neither case is a Subsonic envelope, and a
    /// client must be able to tell "there is no such track" from "the server
    /// just failed".
    pub fn status(&self) -> StatusCode {
        match self {
            StreamError::NotFound => StatusCode::NOT_FOUND,
            StreamError::File(source) if source.kind() == std::io::ErrorKind::NotFound => {
                StatusCode::NOT_FOUND
            }
            StreamError::Library(_) | StreamError::File(_) | StreamError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
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
/// use hearthsound_db::Database;
/// use hearthsound_library::StorageRoots;
///
/// # async fn example(database: Arc<Database>) {
/// let service = hearthsound_api_subsonic::StreamService::new(
///     database,
///     StorageRoots::new(Path::new("/srv/library"), Path::new("/srv/cache")),
///     StreamSessions::new(),
/// );
/// # let _ = service;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct StreamService {
    /// The open database the file lookup and the access record use.
    database: Arc<Database>,
    /// The managed storage roots a file's path resolves against.
    roots: StorageRoots,
    /// The stream sessions an active stream is registered in.
    sessions: StreamSessions,
}

impl StreamService {
    /// Builds the service.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database, for the preferred-file lookup and the
    ///   last-access record.
    /// - `roots`: the managed storage roots a `library` or `cache` file
    ///   resolves against; a file inside a library folder resolves against the
    ///   folder's own path.
    /// - `sessions`: the registry of stream sessions, which must be the same
    ///   registry the activity service reads, or the "no song audio delivered"
    ///   guard cannot see this stream. The composition root takes it from
    ///   [`ActivityService::sessions`](hearthsound_activity::ActivityService::sessions).
    pub fn new(
        database: Arc<Database>,
        roots: StorageRoots,
        sessions: StreamSessions,
    ) -> StreamService {
        StreamService {
            database,
            roots,
            sessions,
        }
    }

    /// Returns the managed storage roots.
    ///
    /// A media file that is not inside a library folder resolves against one of
    /// them, which is how a stored relative path becomes an absolute one.
    pub fn roots(&self) -> &StorageRoots {
        &self.roots
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
    /// and for a track the server has no file for yet, and a server-side error
    /// when the library or the file cannot be read. The caller answers with
    /// [`StreamError::status`], never with a Subsonic envelope.
    pub async fn serve(&self, request: &StreamRequest) -> Result<Response, StreamError> {
        let track = match ClientId::parse(&request.id) {
            Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Track => id,
            Ok(ClientId::Entity(id)) => {
                tracing::debug!(
                    id = %id,
                    "a stream request named a stored entity that is not a track"
                );
                return Err(StreamError::NotFound);
            }
            // A catalog id names an entity the server has not stored; it is the
            // pending-stream case, not a malformed id.
            Ok(ClientId::Catalog(id)) => {
                tracing::debug!(id = %id, "a stream request named an unmaterialized catalog track");
                return self.pending(request).await;
            }
            Err(error) => {
                tracing::debug!(id = %request.id, %error, "a stream request named a malformed id");
                return Err(StreamError::NotFound);
            }
        };

        // The lookup connection is released before the file is served, so a
        // long stream never holds a reader connection.
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
            Some(file) => self.serve_file(request, track, &file).await,
            None if known => self.pending(request).await,
            None => {
                tracing::debug!(track = %track, "a stream request named an unknown track");
                Err(StreamError::NotFound)
            }
        }
    }

    /// Answers a request for a track the server has no file to serve yet.
    ///
    /// This is the extension point for the pending-stream modes. A track whose
    /// file does not exist yet - a catalog track, an acquisition that is still
    /// running, an item that is only available from a provider - is not served
    /// like the file above; the design grows a second path here:
    ///
    /// - the pending mode comes from `stream.pending.mode`, whose `pad` mode
    ///   holds the connection open and feeds the client a silence burst and
    ///   then the file's own audio on the PCM bus, `hold` mode sends nothing
    ///   until the file is ready, and `fail` mode answers `503` with a
    ///   `Retry-After` header;
    /// - `hearthsound_media::PendingStream` and `PendingRequest` are what that
    ///   path drives, and the session registered for it reports
    ///   [`AudioDelivered::probe`] over its `PendingControl`, so silence alone
    ///   is not counted as delivering the song.
    ///
    /// Until that path lands, every request that reaches this function is
    /// answered as HTTP 404, which a client shows as "not found" instead of
    /// playing an error document.
    ///
    /// # Parameters
    ///
    /// - `request`: the request that could not be resolved to a stored file.
    ///
    /// # Errors
    ///
    /// Always returns [`StreamError::NotFound`] until the pending path exists.
    async fn pending(&self, request: &StreamRequest) -> Result<Response, StreamError> {
        tracing::debug!(
            id = %request.id,
            "no stored file to serve for the requested track"
        );
        Err(StreamError::NotFound)
    }

    /// Serves the track's preferred file.
    ///
    /// # Parameters
    ///
    /// - `request`: the request, whose method and conditional headers decide
    ///   the answer.
    /// - `track`: the track the file belongs to, for the stream session.
    /// - `file`: the media file row and the absolute path it resolves to.
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
        let streaming = matches!(parts.status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
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
            delivered_body(body, flag)
        } else {
            Body::new(body)
        };
        Ok(Response::from_parts(parts, body))
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

/// Wraps a body so that its first data frame marks the stream as audibly
/// delivered.
///
/// # Parameters
///
/// - `body`: the body the file service produced.
/// - `flag`: the flag the registered stream session reads.
///
/// # Returns
///
/// The axum body the response carries.
fn delivered_body(body: ServeFileSystemResponseBody, flag: Arc<AtomicBool>) -> Body {
    let stream: BodyDataStream = Body::new(body).into_data_stream();
    Body::from_stream(stream.map(move |frame| {
        if frame.is_ok() {
            flag.store(true, Ordering::SeqCst);
        }
        frame
    }))
}

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
