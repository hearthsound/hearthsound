//! Serving the web client's files.
//!
//! The client is a static site: one entry document, hashed assets under
//! `/_app/immutable/`, and routes the client's own router answers. The server
//! therefore only has to look a request path up in a directory of files, and to
//! answer the entry document for a path that names no file when the request
//! asks for HTML (see [`crate::http`]).
//!
//! The files come from `web.root` when it is set, which is what a development
//! deployment points at a `build/` directory. Without a `web.root`, a client
//! fetched by `cargo xtask fetch-web-client` is embedded at build time: the
//! release pinned in `web-client.lock` is compiled into the binary, and its
//! files carry an `ETag` that answers a matching `If-None-Match` with 304. With
//! neither source there is no client, and every request for one gets the JSON
//! status object or an empty 404. [`WebAssets`] is the one place the source is
//! chosen, so a deployment that serves the client from somewhere else only
//! adds a variant there.
//!
//! Every path is normalized before it is looked up ([`normalize_path`]): it is
//! percent-decoded, split into segments, and rejected when any segment is `.`
//! or `..` or empty or when it holds a backslash. A directory lookup then
//! canonicalizes the file it resolved and refuses anything that is not a
//! regular file below the root, so a symbolic link out of the tree cannot be
//! used to read the filesystem.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;

use crate::error::ServerError;

/// The entry document a client's route falls back to.
const INDEX: &str = "index.html";

/// The path prefix whose files carry a content hash in their name.
///
/// A file under it never changes under the same name, so it can be cached for a
/// year; every other file must be revalidated, which is what makes a new build
/// visible after an upgrade.
const IMMUTABLE_PREFIX: &str = "_app/immutable/";

/// The `Cache-Control` value of a hashed asset.
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";

/// The `Cache-Control` value of everything else.
const NO_CACHE: &str = "no-cache";

/// Where the web client's files come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebAssets {
    /// A directory of files, which is what `web.root` names.
    ///
    /// The path is canonical: it was resolved when the services were built, so
    /// a lookup only canonicalizes the file below it.
    Directory(PathBuf),
    /// The pinned client embedded at build time.
    ///
    /// [`EmbeddedClient`] holds the files `cargo xtask fetch-web-client`
    /// extracted into `target/web-client/` from the release pinned in
    /// `web-client.lock`. An embedded file carries its digest as an `ETag`
    /// and answers a matching `If-None-Match` with 304.
    Embedded,
    /// No client is available.
    ///
    /// The server root answers its JSON status object and a path no other
    /// surface claims is a 404.
    Absent,
}

impl WebAssets {
    /// Resolves the configured `web.root` into an asset source.
    ///
    /// # Parameters
    ///
    /// - `root`: `web.root` from the bootstrap configuration, which is `None`
    ///   when the deployment serves its client from somewhere else.
    ///
    /// # Returns
    ///
    /// [`WebAssets::Directory`] over the canonical root when `root` is set;
    /// otherwise [`WebAssets::Embedded`] when a client was fetched at build
    /// time, and [`WebAssets::Absent`] when neither is available.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::WebRoot`] when `root` is set but cannot be
    /// resolved or does not name a directory: a client directory that does not
    /// exist must stop the server from starting rather than make every page
    /// load fail.
    pub fn resolve(root: Option<&Path>) -> Result<WebAssets, ServerError> {
        let Some(path) = root else {
            if EmbeddedClient::get(INDEX).is_some() {
                tracing::info!("serving the embedded web client");
                return Ok(WebAssets::Embedded);
            }
            return Ok(WebAssets::Absent);
        };
        let canonical = std::fs::canonicalize(path).map_err(|source| ServerError::WebRoot {
            path: path.to_path_buf(),
            source,
        })?;
        if !canonical.is_dir() {
            return Err(ServerError::WebRoot {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "the path is not a directory",
                ),
            });
        }
        tracing::info!(root = %canonical.display(), "serving the web client from a directory");
        Ok(WebAssets::Directory(canonical))
    }

    /// Serves one file of the client.
    ///
    /// # Parameters
    ///
    /// - `path`: an asset path, as [`normalize_path`] returns it: relative, with
    ///   no leading slash.
    /// - `headers`: the request headers; an embedded file whose `ETag` one of
    ///   the `If-None-Match` values names is answered with 304.
    ///
    /// # Returns
    ///
    /// The response, carrying the file's content type and its cache header, or
    /// `None` when this source holds no such file.
    pub async fn serve(&self, path: &str, headers: &HeaderMap) -> Option<Response> {
        match self {
            WebAssets::Directory(root) => serve_file(root, path).await,
            WebAssets::Embedded => serve_embedded(path, headers),
            WebAssets::Absent => None,
        }
    }

    /// Serves the client's entry document.
    ///
    /// # Parameters
    ///
    /// - `headers`: the request headers, as [`WebAssets::serve`] reads them.
    ///
    /// # Returns
    ///
    /// The response for `index.html`, or `None` when this source has no entry
    /// document.
    pub async fn index(&self, headers: &HeaderMap) -> Option<Response> {
        self.serve(INDEX, headers).await
    }
}

/// The web client release embedded at build time.
///
/// `cargo xtask fetch-web-client` extracts the release pinned in
/// `web-client.lock` into `target/web-client/`; a build without that directory
/// embeds nothing and the server answers as [`WebAssets::Absent`]. The
/// `.version` marker the fetch writes is not part of the client.
#[derive(rust_embed::Embed)]
#[folder = "../../target/web-client/"]
#[allow_missing = true]
#[exclude = ".version"]
struct EmbeddedClient;

/// Reads one file below a root and builds its response.
///
/// # Parameters
///
/// - `root`: the canonical directory the client was built into.
/// - `path`: the normalized asset path.
///
/// # Returns
///
/// The response, or `None` when the path does not resolve to a regular file
/// below `root`.
async fn serve_file(root: &Path, path: &str) -> Option<Response> {
    let resolved = tokio::fs::canonicalize(root.join(path)).await.ok()?;
    if !resolved.starts_with(root) {
        tracing::debug!(
            path,
            "refused to serve an asset that resolves outside the web root"
        );
        return None;
    }
    if !tokio::fs::metadata(&resolved).await.ok()?.is_file() {
        return None;
    }
    let body = tokio::fs::read(&resolved).await.ok()?;
    Some(asset_response(path, Body::from(body)))
}

/// Serves one file of the embedded client.
///
/// The file's `ETag` is its SHA-256 in lowercase hex, quoted; a request whose
/// `If-None-Match` names it (or is `*`) gets an empty 304 with the same
/// validators, so a browser revalidating the entry document after a reload
/// costs no body.
///
/// # Parameters
///
/// - `path`: the normalized asset path.
/// - `headers`: the request headers holding `If-None-Match`.
///
/// # Returns
///
/// The response, or `None` when the embedded client has no such file.
fn serve_embedded(path: &str, headers: &HeaderMap) -> Option<Response> {
    let file = EmbeddedClient::get(path)?;
    let etag = format!(
        "\"{}\"",
        data_encoding::HEXLOWER.encode(&file.metadata.sha256_hash())
    );
    let etag_value = HeaderValue::from_str(&etag).ok()?;
    let mut response = if if_none_match(headers, &etag) {
        let mut response = asset_response(path, Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response.headers_mut().remove(header::CONTENT_TYPE);
        response
    } else {
        asset_response(path, Body::from(file.data))
    };
    response.headers_mut().insert(header::ETAG, etag_value);
    Some(response)
}

/// Reports whether a request's `If-None-Match` matches an entity tag.
///
/// Each header holds a comma-separated list of entity tags or `*`; a weak tag
/// (`W/"..."`) matches its strong form, as the weak comparison RFC 9110
/// prescribes for `If-None-Match`.
///
/// # Parameters
///
/// - `headers`: the request headers.
/// - `etag`: the quoted entity tag of the file.
///
/// # Returns
///
/// `true` when any listed tag matches `etag` or the list is `*`.
fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

/// Builds the response for one asset's body.
///
/// # Parameters
///
/// - `path`: the asset path, which names the content type and decides whether
///   the file may be cached for a year.
/// - `body`: the file's bytes.
fn asset_response(path: &str, body: Body) -> Response {
    let content_type = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path.starts_with(IMMUTABLE_PREFIX) {
        IMMUTABLE_CACHE
    } else {
        NO_CACHE
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type.as_ref())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

/// Normalizes a request path into the relative path of an asset.
///
/// The path is percent-decoded first, so an escape cannot hide a separator or a
/// parent reference from the checks below. It is then split into segments, and
/// the request names no asset when any segment is `.`, `..` or empty, when the
/// path holds a backslash (a separator on some systems and never part of an
/// asset name), or when an escape is malformed or does not decode to UTF-8.
///
/// # Parameters
///
/// - `path`: the path of the request, as the URI carries it: percent-encoded
///   and starting with `/`.
///
/// # Returns
///
/// The decoded path without its leading slash, with its segments joined by `/`,
/// or `None` when there is no such asset path.
///
/// # Examples
///
/// ```
/// use hearthsound_server::web::normalize_path;
///
/// assert_eq!(
///     normalize_path("/library/albums").as_deref(),
///     Some("library/albums")
/// );
/// assert_eq!(normalize_path("/_app/immutable/app.js").as_deref(), Some("_app/immutable/app.js"));
/// assert_eq!(normalize_path("/..%2fsecret"), None);
/// assert_eq!(normalize_path("/"), None);
/// ```
pub fn normalize_path(path: &str) -> Option<String> {
    let decoded = percent_decode(path)?;
    if decoded.contains('\\') {
        return None;
    }
    let relative = decoded.strip_prefix('/').unwrap_or(&decoded);
    let mut segments = Vec::new();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
        segments.push(segment);
    }
    Some(segments.join("/"))
}

/// Decodes the `%XX` escapes of a path.
///
/// # Parameters
///
/// - `input`: the raw path.
///
/// # Returns
///
/// The decoded path, or `None` when an escape is not two hexadecimal digits or
/// the decoded bytes are not UTF-8. A path that does not decode is refused
/// rather than served as its literal text, so a mangled request cannot name a
/// file the client never published.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    if !bytes.contains(&b'%') {
        return Some(input.to_owned());
    }
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_digit(*bytes.get(index + 1)?)?;
            let low = hex_digit(*bytes.get(index + 2)?)?;
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Reads one hexadecimal digit.
///
/// # Parameters
///
/// - `byte`: the byte to read.
///
/// # Returns
///
/// The digit's value, or `None` when the byte is not a hexadecimal digit.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
