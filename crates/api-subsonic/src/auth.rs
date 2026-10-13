//! Who is calling: the address a request came from and its credentials.
//!
//! [`Call`] is the extractor a normal endpoint takes. It reads the request's
//! parameters, resolves the credential parameters into a
//! [`Principal`](hearthsound_auth::Principal), and answers a failed check with
//! a failed envelope at HTTP 200, which is what the base protocol mandates.
//! [`BinaryCall`] is the extractor the binary endpoints (`stream`, `download`,
//! `getCoverArt`) take: its rejection is an HTTP status code instead, because a
//! client that treats any 200 body as audio would otherwise play the error
//! document as noise.
//!
//! The address is the peer address from the connection, unless the peer is one
//! of `server.trusted_proxies` and sent an `X-Forwarded-For` header, in which
//! case the header names the client. An untrusted peer's forwarding header is
//! ignored, because a client can write any header it likes.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Deref;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::{FromRequest, Request};
use axum::http::HeaderName;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response as HttpResponse};
use hearthsound_auth::Principal;

use crate::params::{Params, read_values};
use crate::response::{Encoded, Response};
use crate::state::SubsonicState;

/// The header a proxy appends the address it received a request from.
const FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The proxy addresses whose `X-Forwarded-For` header this server trusts.
///
/// An entry is a bare address (`127.0.0.1`, `::1`) or a CIDR block
/// (`10.0.0.0/8`, `2001:db8::/32`).
///
/// # Examples
///
/// ```
/// use std::net::Ipv4Addr;
///
/// use hearthsound_api_subsonic::TrustedProxies;
///
/// let proxies = TrustedProxies::parse(["10.0.0.0/8", "127.0.0.1"]).unwrap();
/// assert!(proxies.trusts(Ipv4Addr::new(10, 1, 2, 3).into()));
/// assert!(!proxies.trusts(Ipv4Addr::new(192, 0, 2, 1).into()));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies {
    /// The trusted networks.
    entries: Vec<Network>,
}

/// One trusted address or CIDR block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Network {
    /// An IPv4 network and its prefix length in bits.
    V4 {
        /// The network address.
        base: u32,
        /// How many leading bits identify the network.
        prefix: u8,
    },
    /// An IPv6 network and its prefix length in bits.
    V6 {
        /// The network address.
        base: u128,
        /// How many leading bits identify the network.
        prefix: u8,
    },
}

/// A trusted-proxy entry that is not an address or a CIDR block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidProxy {
    /// The entry is neither an IP address nor a CIDR block.
    #[error("`{entry}` is not an IP address or a CIDR block")]
    Malformed {
        /// The entry, as the configuration wrote it.
        entry: String,
    },
    /// The entry's prefix length is longer than the address width.
    #[error("`{entry}` has a prefix longer than {width} bits")]
    Prefix {
        /// The entry, as the configuration wrote it.
        entry: String,
        /// The width of the address family, 32 or 128.
        width: u8,
    },
}

impl TrustedProxies {
    /// Parses the entries of `server.trusted_proxies`.
    ///
    /// # Parameters
    ///
    /// - `entries`: the configured addresses and CIDR blocks. An empty entry is
    ///   skipped.
    ///
    /// # Returns
    ///
    /// The proxy list.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProxy`] for the first entry that is not an address or a
    /// CIDR block with a prefix that fits its address family.
    pub fn parse<I, S>(entries: I) -> Result<TrustedProxies, InvalidProxy>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut networks = Vec::new();
        for entry in entries {
            let entry = entry.as_ref().trim();
            if entry.is_empty() {
                continue;
            }
            networks.push(Network::parse(entry)?);
        }
        Ok(TrustedProxies { entries: networks })
    }

    /// Reports whether the list is empty, which trusts no proxy at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Reports whether an address belongs to a trusted proxy.
    ///
    /// # Parameters
    ///
    /// - `address`: the address to look up.
    pub fn trusts(&self, address: IpAddr) -> bool {
        self.entries.iter().any(|network| network.contains(address))
    }

    /// Resolves the address a request came from.
    ///
    /// The forwarding header is consulted only when the peer is itself a
    /// trusted proxy. The chain is then read from its right, skipping the
    /// proxies in the list, and the first address that is not a trusted proxy
    /// is the client; when the whole chain is trusted, its leftmost entry is
    /// returned.
    ///
    /// # Parameters
    ///
    /// - `peer`: the address the connection came from, when the server has
    ///   socket information for it.
    /// - `forwarded_for`: the `X-Forwarded-For` header, when the request
    ///   carried one.
    ///
    /// # Returns
    ///
    /// The client address. Without a peer address the result is the unspecified
    /// address: the server must tell axum where requests come from
    /// (`into_make_service_with_connect_info`), and until it does, forwarding
    /// headers are ignored and every caller shares one rate limit.
    pub fn client_ip(&self, peer: Option<IpAddr>, forwarded_for: Option<&str>) -> IpAddr {
        let Some(peer) = peer else {
            return IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        };
        if !self.trusts(peer) {
            return peer;
        }
        let Some(forwarded_for) = forwarded_for else {
            return peer;
        };

        let chain: Vec<IpAddr> = forwarded_for
            .split(',')
            .filter_map(|hop| hop.trim().parse().ok())
            .collect();
        for address in chain.iter().rev() {
            if !self.trusts(*address) {
                return *address;
            }
        }
        chain.first().copied().unwrap_or(peer)
    }
}

impl Network {
    /// Parses one configured entry.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProxy`] when the address or the prefix does not parse.
    fn parse(entry: &str) -> Result<Network, InvalidProxy> {
        let (address, prefix) = match entry.split_once('/') {
            Some((address, prefix)) => (address.trim(), Some(prefix)),
            None => (entry, None),
        };
        match address.parse::<IpAddr>() {
            Ok(IpAddr::V4(address)) => Ok(Network::V4 {
                base: u32::from(address),
                prefix: prefix_width(prefix, 32, entry)?,
            }),
            Ok(IpAddr::V6(address)) => Ok(Network::V6 {
                base: u128::from(address),
                prefix: prefix_width(prefix, 128, entry)?,
            }),
            Err(_) => Err(InvalidProxy::Malformed {
                entry: entry.to_owned(),
            }),
        }
    }

    /// Reports whether an address is inside the network.
    fn contains(self, address: IpAddr) -> bool {
        match (self, address) {
            (Network::V4 { base, prefix }, IpAddr::V4(address)) => {
                let mask = mask32(prefix);
                u32::from(address) & mask == base & mask
            }
            (Network::V6 { base, prefix }, IpAddr::V6(address)) => {
                let mask = mask128(prefix);
                u128::from(address) & mask == base & mask
            }
            _ => false,
        }
    }
}

/// Returns the prefix length of an entry, defaulting to the full width.
///
/// # Errors
///
/// Returns [`InvalidProxy::Prefix`] when the prefix is not a number or does not
/// fit the address family.
fn prefix_width(prefix: Option<&str>, width: u8, entry: &str) -> Result<u8, InvalidProxy> {
    match prefix.map(str::trim) {
        None => Ok(width),
        Some(text) => match text.parse::<u8>() {
            Ok(value) if value <= width => Ok(value),
            _ => Err(InvalidProxy::Prefix {
                entry: entry.to_owned(),
                width,
            }),
        },
    }
}

/// Returns the IPv4 mask of a prefix length.
fn mask32(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        prefix => u32::MAX << (32 - prefix),
    }
}

/// Returns the IPv6 mask of a prefix length.
fn mask128(prefix: u8) -> u128 {
    match prefix {
        0 => 0,
        prefix => u128::MAX << (128 - prefix),
    }
}

/// A `/rest` call with its principal and parameters.
///
/// A handler takes this as its last argument; it dereferences to the call's
/// [`Params`], so an endpoint reads a parameter as `call.get("id")` and
/// authenticates nothing itself.
#[derive(Debug, Clone)]
pub struct Call {
    /// The shared services.
    state: SubsonicState,
    /// The authenticated principal.
    principal: Principal,
    /// The request's parameters.
    params: Params,
}

impl Call {
    /// Returns the shared services.
    pub fn state(&self) -> &SubsonicState {
        &self.state
    }

    /// Returns the authenticated principal.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns the request's parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Renders a response in the encoding the client asked for.
    ///
    /// # Parameters
    ///
    /// - `response`: the response to render.
    ///
    /// # Returns
    ///
    /// The encoded response, ready to be returned from the handler.
    pub fn encode(&self, response: Response) -> Encoded {
        self.params.encode(response)
    }
}

impl Deref for Call {
    type Target = Params;

    /// Borrows the call's parameters, so a handler can read them directly.
    fn deref(&self) -> &Params {
        &self.params
    }
}

impl FromRequest<SubsonicState> for Call {
    type Rejection = HttpResponse;

    /// Parses a request's parameters and resolves its credentials.
    ///
    /// # Errors
    ///
    /// Returns a bare HTTP status code when the request's form body cannot be
    /// read, and a failed envelope at HTTP 200 with the credential error's code
    /// when the credentials are absent, contradictory, or wrong.
    async fn from_request(request: Request, state: &SubsonicState) -> Result<Call, HttpResponse> {
        let (parts, body) = request.into_parts();
        let params = match read_values(&parts, body).await {
            Ok(values) => Params::from_values(values),
            Err(rejection) => return Err(rejection.into_response()),
        };
        match authenticate(state, &parts, &params).await {
            Ok(principal) => Ok(Call {
                state: state.clone(),
                principal,
                params,
            }),
            Err(error) => Err(params.encode(Response::failed(error)).into_response()),
        }
    }
}

/// A `/rest` call to a binary endpoint with its principal and parameters.
///
/// The three endpoints that answer with media bytes take this instead of
/// [`Call`], because their failures are HTTP status codes rather than
/// envelopes: a client that plays any 200 body would otherwise play an error
/// document as noise.
///
/// A binary handler answers a permission refusal with [`forbidden`], and its own
/// not-found and unavailable cases with the matching status code.
///
/// Unlike [`Call`], a binary call keeps the request's method and headers,
/// because what these endpoints answer depends on them: `stream` serves
/// `Range` requests and revalidates with the `If-*` headers, and `getCoverArt`
/// revalidates the same way. A handler reads them with
/// [`BinaryCall::method`] and [`BinaryCall::headers`].
#[derive(Debug, Clone)]
pub struct BinaryCall {
    /// The shared services.
    state: SubsonicState,
    /// The authenticated principal.
    principal: Principal,
    /// The request's parameters.
    params: Params,
    /// The request's method.
    method: Method,
    /// The request's headers.
    headers: HeaderMap,
}

impl BinaryCall {
    /// Returns the shared services.
    pub fn state(&self) -> &SubsonicState {
        &self.state
    }

    /// Returns the authenticated principal.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns the request's parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Returns the request's method.
    ///
    /// A binary endpoint serves the same bytes for `GET` and `HEAD`; the method
    /// tells it whether the body is wanted, which is what `stream` uses to
    /// decide whether it starts a stream session.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Returns the request's headers.
    ///
    /// The `Range` and `If-*` headers live here, so a binary endpoint reads its
    /// conditional requests from the request rather than from the parameters.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

impl Deref for BinaryCall {
    type Target = Params;

    /// Borrows the call's parameters, so a handler can read them directly.
    fn deref(&self) -> &Params {
        &self.params
    }
}

impl FromRequest<SubsonicState> for BinaryCall {
    type Rejection = HttpResponse;

    /// Parses a request's parameters and resolves its credentials.
    ///
    /// # Errors
    ///
    /// Returns HTTP 400 or 413 when the request's form body cannot be read,
    /// HTTP 401 when the credentials are absent, contradictory, or wrong, and
    /// HTTP 500 when the check itself could not be completed. The body is always
    /// empty, for the reason [`BinaryCall`] exists.
    async fn from_request(
        request: Request,
        state: &SubsonicState,
    ) -> Result<BinaryCall, HttpResponse> {
        let (parts, body) = request.into_parts();
        let params = match read_values(&parts, body).await {
            Ok(values) => Params::from_values(values),
            Err(rejection) => return Err(rejection.into_response()),
        };
        match authenticate(state, &parts, &params).await {
            Ok(principal) => Ok(BinaryCall {
                state: state.clone(),
                principal,
                params,
                method: parts.method.clone(),
                headers: parts.headers.clone(),
            }),
            Err(error) => Err(binary_rejection(&error)),
        }
    }
}

/// Answers that the principal may not have the bytes it asked for.
///
/// A binary endpoint calls this when the request is authenticated and refers to
/// an entity the principal's permissions do not cover; the alternative, a failed
/// envelope, is forbidden for these endpoints. An entity that does not exist is
/// HTTP 404 instead, and one that is not available yet is HTTP 503 with a
/// `Retry-After` header.
///
/// # Returns
///
/// HTTP 403 with an empty body.
pub fn forbidden() -> HttpResponse {
    StatusCode::FORBIDDEN.into_response()
}

/// Resolves the credentials of a request into a principal.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `parts`: the request's head, which supplies the peer address and the
///   forwarding header.
/// - `params`: the request's parameters.
///
/// # Errors
///
/// Returns the error to report when the credentials are absent, contradictory,
/// or wrong, and a generic error when the check itself could not be completed.
async fn authenticate(
    state: &SubsonicState,
    parts: &Parts,
    params: &Params,
) -> Result<Principal, crate::response::SubsonicError> {
    state
        .authenticate(
            client_ip(parts, state.trusted_proxies()),
            &params.credentials(),
        )
        .await
}

/// Returns the status a rejected binary call is answered with.
///
/// A credential rejection is HTTP 401; a generic error means the check itself
/// failed, which is HTTP 500. Neither carries a body.
fn binary_rejection(error: &crate::response::SubsonicError) -> HttpResponse {
    if error.code() == crate::response::codes::GENERIC {
        tracing::error!("a binary /rest call could not be authenticated");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tracing::debug!(
        code = error.code(),
        "rejected the credentials of a binary /rest call"
    );
    StatusCode::UNAUTHORIZED.into_response()
}

/// Returns the address a request came from.
///
/// The peer address comes from the connection information axum was built with,
/// and the forwarding header is trusted only when that peer is a trusted proxy.
fn client_ip(parts: &Parts, trusted: &TrustedProxies) -> IpAddr {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let forwarded_for = parts
        .headers
        .get(&FORWARDED_FOR)
        .and_then(|value| value.to_str().ok());
    trusted.client_ip(peer, forwarded_for)
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    use super::*;

    /// Builds the request head of a request that carries the headers.
    fn parts(headers: &[(&str, &str)]) -> Parts {
        let mut builder = HttpRequest::builder().uri("/rest/ping");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap().into_parts().0
    }

    #[test]
    fn an_entry_is_an_address_or_a_cidr_block() {
        let proxies =
            TrustedProxies::parse(["10.0.0.0/8", "127.0.0.1", "2001:db8::/32", "::1"]).unwrap();
        assert!(!proxies.is_empty());
        assert!(proxies.trusts(Ipv4Addr::new(10, 255, 0, 1).into()));
        assert!(proxies.trusts(Ipv4Addr::new(127, 0, 0, 1).into()));
        assert!(proxies.trusts("2001:db8::ffff".parse::<Ipv6Addr>().unwrap().into()));
        assert!(proxies.trusts(Ipv6Addr::LOCALHOST.into()));
        assert!(!proxies.trusts(Ipv4Addr::new(11, 0, 0, 1).into()));
        assert!(!proxies.trusts(Ipv4Addr::new(127, 0, 0, 2).into()));
        assert!(!proxies.trusts("2001:db9::1".parse::<Ipv6Addr>().unwrap().into()));
        assert!(TrustedProxies::parse([""]).unwrap().is_empty());
    }

    #[test]
    fn a_full_mask_matches_exactly_one_address() {
        let proxies = TrustedProxies::parse(["192.0.2.7/32", "2001:db8::1/128"]).unwrap();
        assert!(proxies.trusts(Ipv4Addr::new(192, 0, 2, 7).into()));
        assert!(!proxies.trusts(Ipv4Addr::new(192, 0, 2, 8).into()));
        assert!(proxies.trusts("2001:db8::1".parse::<Ipv6Addr>().unwrap().into()));
        assert!(!proxies.trusts("2001:db8::2".parse::<Ipv6Addr>().unwrap().into()));
    }

    #[test]
    fn a_zero_prefix_matches_a_whole_address_family_and_not_the_other() {
        let proxies = TrustedProxies::parse(["0.0.0.0/0"]).unwrap();
        assert!(proxies.trusts(Ipv4Addr::new(203, 0, 113, 9).into()));
        assert!(!proxies.trusts(Ipv6Addr::LOCALHOST.into()));
    }

    #[test]
    fn an_entry_that_is_not_an_address_is_rejected() {
        assert_eq!(
            TrustedProxies::parse(["proxy.internal"]).unwrap_err(),
            InvalidProxy::Malformed {
                entry: "proxy.internal".to_owned()
            }
        );
        assert_eq!(
            TrustedProxies::parse(["10.0.0.0/64"]).unwrap_err(),
            InvalidProxy::Prefix {
                entry: "10.0.0.0/64".to_owned(),
                width: 32
            }
        );
        assert_eq!(
            TrustedProxies::parse(["2001:db8::/129"]).unwrap_err(),
            InvalidProxy::Prefix {
                entry: "2001:db8::/129".to_owned(),
                width: 128
            }
        );
        assert!(TrustedProxies::parse(["10.0.0.0/8", "10.0.0.0"]).is_ok());
    }

    #[test]
    fn an_untrusted_peer_keeps_its_own_address() {
        let proxies = TrustedProxies::parse(["10.0.0.0/8"]).unwrap();
        let peer = Ipv4Addr::new(203, 0, 113, 9);
        assert_eq!(
            proxies.client_ip(Some(peer.into()), Some("198.51.100.4")),
            IpAddr::V4(peer)
        );
        assert_eq!(proxies.client_ip(Some(peer.into()), None), IpAddr::V4(peer));
    }

    #[test]
    fn a_trusted_peer_is_replaced_by_the_first_untrusted_hop() {
        let proxies = TrustedProxies::parse(["10.0.0.0/8"]).unwrap();
        let peer = Ipv4Addr::new(10, 0, 0, 1);
        assert_eq!(
            proxies.client_ip(Some(peer.into()), Some("198.51.100.4, 10.1.2.3")),
            "198.51.100.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            proxies.client_ip(Some(peer.into()), Some("198.51.100.4")),
            "198.51.100.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            proxies.client_ip(Some(peer.into()), Some("198.51.100.4, 10.1.2.3, 10.9.9.9")),
            "198.51.100.4".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            proxies.client_ip(Some(peer.into()), Some("not-an-address, 10.1.2.3")),
            "10.1.2.3".parse::<IpAddr>().unwrap(),
            "a hop that is not an address is skipped"
        );
    }

    #[test]
    fn a_chain_of_trusted_proxies_falls_back_to_its_leftmost_hop() {
        let proxies = TrustedProxies::parse(["10.0.0.0/8", "192.168.0.0/16"]).unwrap();
        assert_eq!(
            proxies.client_ip(Some(Ipv4Addr::new(10, 0, 0, 1).into()), Some("192.168.1.1")),
            "192.168.1.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn without_a_peer_address_every_caller_shares_one_address() {
        let proxies = TrustedProxies::parse(["10.0.0.0/8"]).unwrap();
        assert_eq!(
            proxies.client_ip(None, Some("198.51.100.4")),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            TrustedProxies::default().client_ip(None, None),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn the_request_head_supplies_the_peer_and_the_forwarding_header() {
        let proxies = TrustedProxies::parse(["10.0.0.0/8"]).unwrap();
        let mut parts = parts(&[("x-forwarded-for", "198.51.100.4")]);
        assert_eq!(
            client_ip(&parts, &proxies),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            "without connection information the header must not be believed"
        );

        parts
            .extensions
            .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 5000))));
        assert_eq!(
            client_ip(&parts, &proxies),
            "198.51.100.4".parse::<IpAddr>().unwrap()
        );

        parts
            .extensions
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 5000))));
        assert_eq!(
            client_ip(&parts, &proxies),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))
        );
    }
}
