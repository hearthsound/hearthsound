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
//! # Errors: envelopes versus binary endpoints
//!
//! Normal endpoints answer errors with HTTP 200 and a failed envelope, because
//! that is what the base protocol mandates. The binary endpoints must not, for
//! the reason [`BinaryCall`] exists.

#![forbid(unsafe_code)]

mod auth;
mod params;
mod render;
mod response;
mod routes;
mod state;
mod value;

pub use auth::{BinaryCall, Call, InvalidProxy, TrustedProxies, forbidden};
pub use params::{MAX_FORM_BODY_BYTES, Params, ParamsRejection};
pub use response::{
    API_VERSION, Encoded, Response, ResponseFormat, SERVER_TYPE, SERVER_VERSION, Status,
    SubsonicError, codes,
};
pub use routes::{register, router, routes};
pub use state::SubsonicState;
pub use value::{Array, Object, Value};
