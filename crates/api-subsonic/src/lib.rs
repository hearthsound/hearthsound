//! The Subsonic-compatible `/rest` surface.
//!
//! Hearthsound answers the Subsonic and OpenSubsonic protocol on `/rest`. This
//! crate owns that surface: the envelope every call is wrapped in, the
//! credential check in front of each endpoint, and the endpoints themselves.
//!
//! # The envelope
//!
//! Every non-binary call answers HTTP 200 with a [`Response`]: a `status`, the
//! base protocol version this server claims, the server type and version, the
//! OpenSubsonic marker, and either the call's payload members or an
//! [`SubsonicError`]. The payload is built from a small ordered value model
//! ([`Object`], [`Array`], and scalars), and one set of members is rendered to
//! both encodings, so the field coverage of the two is identical by
//! construction.
//!
//! XML is the default encoding, as the base protocol defines it; `f=json` and
//! `f=jsonp` select JSON (`f=jsonp` is answered with plain JSON, because
//! callback wrapping is not implemented). A request whose `f` is absent or
//! unrecognized is answered with XML.
//!
//! # The two encodings
//!
//! Both encodings are written from the same values, with these rules:
//!
//! - An [`Object`] becomes one element. Its scalar members become attributes,
//!   its object and array members become child elements, and both keep the
//!   order they were built in, so a response is byte-stable.
//! - An [`Array`] becomes one element per item, named after the member it is
//!   set as. A member that is an array of objects therefore renders as repeated
//!   elements directly inside the enclosing element, which is what the base
//!   protocol's schema prescribes (`musicFolders` holds repeated
//!   `musicFolder` elements). [`Array::named`] overrides the element name for
//!   the members whose JSON name is plural while the schema's element is
//!   singular (`openSubsonicExtensions` holds repeated
//!   `openSubsonicExtension` elements).
//! - An empty array renders as no elements at all; an element with no
//!   attributes and no children renders as a self-closing tag.
//! - An XML element's name is used as written, so member names must be valid
//!   XML names. Values are escaped for both encodings.
//!
//! # Requests
//!
//! A handler takes a [`Call`] as its last argument: it carries the
//! authenticated [`Principal`](hearthsound_auth::Principal), the request's
//! [`Params`], and the shared [`SubsonicState`]. Parameters are read from the
//! query string and from the body of an `application/x-www-form-urlencoded`
//! POST (the `formPost` extension), and the credentials (`u`, `p`, `t`, `s`,
//! `apiKey`) are resolved by `hearthsound-auth` into a principal whose
//! permissions the handler checks. A failed credential check answers a failed
//! envelope at HTTP 200 with the credential error's code and help URL.
//!
//! [`BinaryCall`] is the extractor for the three endpoints that answer with
//! media bytes (`stream`, `download`, `getCoverArt`). Its credential failures
//! are HTTP status codes rather than envelopes, because a client that treats
//! any 200 body as audio would otherwise play an error document as noise; a
//! handler that refuses on permissions uses [`forbidden`].
//!
//! Every endpoint is registered with [`register`], which serves it at both
//! `/rest/<name>` and `/rest/<name>.view` for GET and POST. Everything under
//! `/rest/` that no endpoint matched, including the endpoint families this
//! server never implements, answers Subsonic error 70.
//!
//! The address a request is attributed to is the peer address from the
//! connection, unless that peer is one of `server.trusted_proxies` and sent an
//! `X-Forwarded-For` header; the header of an untrusted peer is ignored, because
//! any client can write it. [`TrustedProxies`] is built from the bootstrap
//! configuration and handed to [`SubsonicState::with_trusted_proxies`].
//!
//! # Endpoints
//!
//! | Endpoint | Notes |
//! |---|---|
//! | `ping` | liveness within the protocol |
//! | `getLicense` | always valid, because Hearthsound has no license tiers |
//! | `getOpenSubsonicExtensions` | public, no credentials; advertises `apiKeyAuthentication` 1 and `formPost` 1 |
//! | `tokenInfo` | the user an API key belongs to |
//! | `getMusicFolders` | the personal library, and the server unless `privacy.server_browse` is disabled |
//! | `getArtists` | the artist index over the accessible scope |
//! | `getArtist` | one artist with its albums |
//! | `getAlbum` | one album with its songs |
//! | `getSong` | one song |
//! | `getAlbumList2` | the albums in scope, ordered by `newest`, `alphabeticalByName`, or `random` |
//! | `search3` | local matches first, then catalog matches when `search.catalog.enabled` |
//! | `stream` | the track's media bytes: the stored file with ranges and validators, or a transcode |
//! | `scrobble` | records a play for a stored track; now-playing is accepted and discarded |
//!
//! The browse endpoints share one scope rule: an explicit `musicFolderId` of
//! `1` is the requesting user's personal library and of `2` the server-wide
//! view, and a request without one reads the user's `subsonic.default_scope`.
//!
//! `search3`, `getSong`, `getAlbum`, and `getArtist` also answer for entities
//! the server has not stored: a catalog provider's entity is projected with an
//! `x_` id, so the client can stream, save, or acquire it later. The catalog
//! layer is a dependency of this crate for exactly that reason, and it never
//! makes a search fail: a catalog instance that is down leaves the local
//! results, while a single-entity catalog lookup answers with the provider's
//! failure.
//!
//! Every other path under `/rest/`, including the endpoint families this server
//! never implements (podcast, internet radio, share, video, and chat), answers
//! Subsonic error 70.
//!
//! # Adding an endpoint
//!
//! 1. Write the handler in `src/endpoints/`: an `async fn` whose last argument
//!    is a [`Call`], or a [`BinaryCall`] for an endpoint that answers with media
//!    bytes, and whose return type is [`Encoded`]. A handler that needs the
//!    shared services takes the extractor's [`Call::state`].
//! 2. Build the payload from [`Object`], [`Array`], and scalars, in the order the
//!    protocol's example shows. One value tree renders to both encodings, so the
//!    field coverage cannot drift between them.
//! 3. Register the handler in [`routes`] with [`register`], which serves it at
//!    `/rest/<name>` and `/rest/<name>.view` for GET and POST.
//! 4. Snapshot both encodings in `tests/`, and cover the credential failure
//!    through the router.
//!
//! Two rules from the design bind every endpoint. A field a server supports is
//! emitted with its default value rather than omitted, so a client can tell a
//! supported-but-unknown value from an unsupported field. And an endpoint or a
//! field that is not part of the base protocol appears only when the request
//! carries the vendor opt-in that [`Params::vendor_enabled`] reports, and is
//! advertised in `getOpenSubsonicExtensions` before any client can rely on it.
//!
//! # Errors: envelopes versus binary endpoints
//!
//! Normal endpoints answer errors with HTTP 200 and a failed envelope, because
//! that is what the base protocol mandates. The binary endpoints (`stream`,
//! `download`, and `getCoverArt`) must not, for the reason [`BinaryCall`]
//! exists: a client that treats any 200 body as audio would play an error
//! document as noise.

#![forbid(unsafe_code)]

mod auth;
mod browse;
mod catalog;
mod endpoints;
mod params;
mod projection;
mod render;
mod response;
mod routes;
mod scope;
mod state;
mod stream;
mod value;

pub use auth::{BinaryCall, Call, InvalidProxy, TrustedProxies, forbidden};
pub use endpoints::{
    get_album, get_album_list_2, get_artist, get_artists, get_license, get_music_folders,
    get_open_subsonic_extensions, get_song, ping, scrobble, search3, stream, token_info,
};
pub use params::{MAX_FORM_BODY_BYTES, Params, ParamsRejection};
pub use response::{
    API_VERSION, Encoded, Response, ResponseFormat, SERVER_TYPE, SERVER_VERSION, Status,
    SubsonicError, codes,
};
pub use routes::{register, router, routes};
pub use state::SubsonicState;
pub use stream::{StreamError, StreamFormat, StreamRequest, StreamService};
pub use value::{Array, Object, Value};
