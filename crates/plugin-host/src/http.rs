//! The `host-http` import: outbound HTTP on behalf of one plugin instance.
//!
//! Every request a plugin makes is filtered by the instance's approved HTTP
//! permission before any bytes leave the process. The checks, in order, are:
//!
//! 1. **Allowlist.** Only hosts the instance was approved for are reachable.
//!    An entry is a literal hostname, a `*.domain` wildcard matching one or more
//!    labels below the domain, or a `$config:<field>` reference naming the host
//!    of the URL stored in that server-configuration field.
//! 2. **Address policy.** A literal or wildcard entry must resolve to public
//!    addresses only: a name that resolves to a private, loopback, link-local,
//!    carrier-grade NAT, multicast, unspecified, documentation or unique-local
//!    address is denied, which blocks DNS rebinding into the server's own
//!    network. A `$config` host may resolve anywhere, because the administrator
//!    chose that address. The policy is applied to the addresses the connection
//!    actually uses: the host is resolved here and the result is pinned into the
//!    HTTP client's resolver, so the client never resolves the name a second
//!    time between the check and the connect.
//! 3. **Timeout.** The plugin may ask for a shorter timeout; the default is
//!    10 seconds and the ceiling is 60 seconds. The effective budget is also
//!    clamped to whatever is left of the plugin call's deadline.
//! 4. **Redirects.** At most five, every hop re-checked against the allowlist
//!    and the address policy. A hop to another origin drops the request's
//!    credential headers (`authorization`, `proxy-authorization`, `cookie`) and
//!    a 301, 302 or 303 rewrites the method to GET without the body, so a
//!    redirect cannot carry a plugin's API key to a second host.
//! 5. **Body cap.** The response body is capped (10 MiB by default, and at most
//!    64 MiB when the manifest asked for more) while it streams, so an
//!    oversized response fails instead of being copied into guest memory.
//! 6. **User-Agent.** When the plugin sends none, the host identifies itself as
//!    `Hearthsound/<version> (+https://hearthsound.app) plugin/<id>/<version>`.
//! 7. **Rate limit.** The instance's `rateLimit {perSecond, burst}` is enforced
//!    with a token bucket that waits, but never past the call deadline.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use hearthsound_core::Slug;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::{Client, Method, Url};

use crate::manifest::HttpPermission;

/// The timeout applied to a request that does not ask for one.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// The largest timeout a request may ask for.
pub const MAX_TIMEOUT_MS: u64 = 60_000;

/// The response body cap applied when the manifest asks for none.
pub const DEFAULT_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// The largest response body cap a manifest may request.
pub const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// The number of redirects a single request may follow.
pub const MAX_REDIRECTS: usize = 5;

/// One outbound request, as the plugin asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// The HTTP method, for example `GET`.
    pub method: String,
    /// The absolute request URL.
    pub url: String,
    /// The request headers, in order.
    pub headers: Vec<(String, String)>,
    /// The request body, when the method carries one.
    pub body: Option<Vec<u8>>,
    /// The timeout the plugin asked for, in milliseconds.
    pub timeout_ms: Option<u32>,
}

/// One outbound response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The response headers, in the order the server sent them.
    pub headers: Vec<(String, String)>,
    /// The response body.
    pub body: Vec<u8>,
}

/// Why an outbound request did not complete.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// The host denied the request before sending it.
    #[error("{0}")]
    Denied(String),
    /// The request did not finish inside its budget.
    #[error("the request exceeded its timeout")]
    Timeout,
    /// The response body exceeded the instance's cap of `{0}` bytes.
    #[error("the response body exceeded the {0} byte cap")]
    BodyTooLarge(u64),
    /// The request could not be sent or the connection failed.
    #[error("{0}")]
    Network(String),
}

/// One entry of an instance's HTTP allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostRule {
    /// An exact hostname.
    Literal(String),
    /// A `*.domain` wildcard, matching one or more labels below the domain.
    Wildcard(String),
    /// The host of the URL in one server-configuration field.
    Config {
        /// The configuration field the host came from.
        field: String,
        /// The host the field's URL names, when the field is present and holds
        /// a URL.
        host: Option<String>,
    },
}

/// What one instance is approved to reach over HTTP.
///
/// Built from the plugin's approved `http` permission and the instance's
/// server-scoped configuration, which resolves any `$config:` allowlist entry.
/// An instance whose plugin has no `http` permission gets an empty grant and
/// every request is denied.
#[derive(Debug, Clone)]
pub struct HttpGrant {
    rules: Vec<HostRule>,
    /// One slot per rule: the limit that rule declared for its own host.
    host_rates: Vec<Option<(NonZeroU32, NonZeroU32)>>,
    rate_limit: Option<(NonZeroU32, NonZeroU32)>,
    max_body_bytes: u64,
}

impl HttpGrant {
    /// Builds a grant from an approved permission and a server configuration.
    ///
    /// # Parameters
    ///
    /// - `permission`: the instance's approved `http` permission, or `None` when
    ///   the plugin was not approved for HTTP at all.
    /// - `server_config`: the instance's server-scoped configuration as JSON
    ///   text, used to resolve `$config:<field>` allowlist entries. A field that
    ///   is absent, is not a string, or does not hold an absolute URL, resolves
    ///   to no host, so the entry matches nothing.
    pub fn new(permission: Option<&HttpPermission>, server_config: &str) -> HttpGrant {
        let Some(permission) = permission else {
            return HttpGrant {
                rules: Vec::new(),
                host_rates: Vec::new(),
                rate_limit: None,
                max_body_bytes: DEFAULT_BODY_BYTES,
            };
        };

        let config: serde_json::Value =
            serde_json::from_str(server_config).unwrap_or(serde_json::Value::Null);
        let rules = permission
            .hosts
            .iter()
            .map(|entry| match entry.host().strip_prefix("$config:") {
                Some(field) => HostRule::Config {
                    field: field.to_owned(),
                    host: config_host(&config, field),
                },
                None => match entry.host().strip_prefix("*.") {
                    Some(domain) => HostRule::Wildcard(domain.to_ascii_lowercase()),
                    None => HostRule::Literal(entry.host().to_ascii_lowercase()),
                },
            })
            .collect();

        let host_rates = permission
            .hosts
            .iter()
            .map(|entry| {
                entry.rate_limit().map(|limit| {
                    (
                        NonZeroU32::new(limit.per_second).unwrap_or(NonZeroU32::MIN),
                        NonZeroU32::new(limit.burst).unwrap_or(NonZeroU32::MIN),
                    )
                })
            })
            .collect();
        let rate_limit = permission.rate_limit.map(|limit| {
            (
                NonZeroU32::new(limit.per_second).unwrap_or(NonZeroU32::MIN),
                NonZeroU32::new(limit.burst).unwrap_or(NonZeroU32::MIN),
            )
        });
        let max_body_bytes = permission
            .max_body_bytes
            .map(|bytes| bytes.min(MAX_BODY_BYTES))
            .unwrap_or(DEFAULT_BODY_BYTES);

        HttpGrant {
            rules,
            host_rates,
            rate_limit,
            max_body_bytes,
        }
    }

    /// Returns the index of the first allowlist rule that matches `host`.
    fn matched(&self, host: &str) -> Option<usize> {
        self.rules.iter().position(|rule| match rule {
            HostRule::Literal(literal) => literal == host,
            HostRule::Wildcard(domain) => host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.')),
            HostRule::Config {
                host: Some(config), ..
            } => config == host,
            HostRule::Config { host: None, .. } => false,
        })
    }

    /// Returns whether the grant approves no host at all.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Returns whether `host` matches an allowlist entry.
    pub fn allows(&self, host: &str) -> bool {
        self.rules.iter().any(|rule| match rule {
            HostRule::Literal(literal) => literal == host,
            HostRule::Wildcard(domain) => host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.')),
            HostRule::Config {
                host: Some(config), ..
            } => config == host,
            HostRule::Config { host: None, .. } => false,
        })
    }

    /// Returns whether `host` came from a `$config:` entry, which may resolve
    /// to any address.
    pub fn may_resolve_anywhere(&self, host: &str) -> bool {
        self.rules.iter().any(|rule| match rule {
            HostRule::Config {
                host: Some(config), ..
            } => config == host,
            _ => false,
        })
    }

    /// Describes why `host` did not match the allowlist.
    pub fn denial(&self, host: &str) -> String {
        if self.rules.is_empty() {
            return format!("host-http is not approved for this instance, so `{host}` is denied");
        }
        let mut names: Vec<String> = self
            .rules
            .iter()
            .map(|rule| match rule {
                HostRule::Literal(literal) => literal.clone(),
                HostRule::Wildcard(domain) => format!("*.{domain}"),
                HostRule::Config { field, host } => match host {
                    Some(host) => format!("$config:{field} ({host})"),
                    None => format!("$config:{field} (unset)"),
                },
            })
            .collect();
        names.sort();
        format!(
            "host `{host}` is not in the approved allowlist [{}]",
            names.join(", ")
        )
    }
}

/// Resolves the host of the URL in one configuration field.
///
/// # Parameters
///
/// - `config`: the parsed server configuration.
/// - `field`: the field name from a `$config:` allowlist entry.
///
/// # Returns
///
/// The lowercased host of the field's URL, or `None` when the field is absent,
/// is not a string, or is not an absolute URL with a host.
fn config_host(config: &serde_json::Value, field: &str) -> Option<String> {
    let url = config.get(field)?.as_str()?;
    let url = Url::parse(url).ok()?;
    Some(url.host_str()?.to_ascii_lowercase())
}

/// The HTTP client of one plugin instance.
///
/// One client per instance keeps its approved allowlist, its rate limiter and
/// its address pins together, and lets the connection pool be reused across
/// calls. A client is cheap to hold but not to build, so it is built once per
/// instance and shared by the instance's stores.
pub struct HttpClient {
    slug: Slug,
    grant: HttpGrant,
    client: Client,
    rates: RateLimiters,
    pins: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>>,
}

/// A [`HttpClient`] shared by every store and provider of one instance.
///
/// The engines' store pools and the provider adapters both need a client, and
/// they are built at different times from the same instance configuration.
/// Holding the client here lets the provider registry hand one client to both,
/// so the instance has one rate limiter and one connection pool rather than one
/// per product of the instance, the layer and the pool size.
#[derive(Clone)]
pub(crate) struct SharedHttpClient(pub(crate) Arc<HttpClient>);

impl PartialEq for SharedHttpClient {
    /// Compares the clients by identity: one instance's client is the same
    /// client everywhere it is passed, and two separately built clients differ
    /// even when their configuration is equal, because their rate limiters do.
    fn eq(&self, other: &SharedHttpClient) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for SharedHttpClient {
    /// Prints an opaque placeholder: the client holds a connection pool and a
    /// rate limiter, neither of which is useful in a log line.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SharedHttpClient(..)")
    }
}

/// The rate limiters of one instance.
///
/// The instance-wide limiter holds every host whose allowlist entry declares no
/// limit of its own; a host whose entry does is held to that entry's limiter
/// alone, so one slow service cannot spend the budget of the rest.
pub(crate) struct RateLimiters {
    instance: Option<DefaultDirectRateLimiter>,
    /// One slot per allowlist rule, for the rules that declared a limit.
    hosts: Vec<Option<DefaultDirectRateLimiter>>,
}

impl RateLimiters {
    /// Returns the limiter `host` waits on, when it waits on one at all.
    ///
    /// # Parameters
    ///
    /// - `grant`: the grant the request was checked against.
    /// - `host`: the lowercased host of the hop.
    fn for_host(&self, grant: &HttpGrant, host: &str) -> Option<&DefaultDirectRateLimiter> {
        grant
            .matched(host)
            .and_then(|index| self.hosts.get(index))
            .and_then(Option::as_ref)
            .or(self.instance.as_ref())
    }
}

impl HttpClient {
    /// Builds the client of one instance.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the plugin id, used in the User-Agent.
    /// - `plugin_version`: the installed plugin version, used in the
    ///   User-Agent.
    /// - `slug`: the instance slug, used in denial messages and logs.
    /// - `grant`: the instance's approved HTTP grant.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Network`] when the HTTP client cannot be built.
    pub fn new(
        plugin: String,
        plugin_version: &str,
        slug: Slug,
        grant: HttpGrant,
    ) -> Result<HttpClient, HttpError> {
        let pins = Arc::new(Mutex::new(HashMap::new()));
        let user_agent = format!(
            "Hearthsound/{} (+https://hearthsound.app) plugin/{}/{}",
            env!("CARGO_PKG_VERSION"),
            plugin,
            plugin_version
        );
        let client = pinned_client(Arc::clone(&pins), user_agent, Some(slug.clone()))?;

        let limiter = |(per_second, burst): (NonZeroU32, NonZeroU32)| {
            RateLimiter::direct(Quota::per_second(per_second).allow_burst(burst))
        };
        let rates = RateLimiters {
            instance: grant.rate_limit.map(limiter),
            hosts: grant
                .host_rates
                .iter()
                .map(|rate| rate.map(limiter))
                .collect(),
        };

        Ok(HttpClient {
            slug,
            grant,
            client,
            rates,
            pins,
        })
    }

    /// Returns the instance slug the client belongs to.
    pub fn slug(&self) -> &Slug {
        &self.slug
    }

    /// Returns the instance's grant.
    pub fn grant(&self) -> &HttpGrant {
        &self.grant
    }

    /// Performs one request, enforcing the instance's grant.
    ///
    /// # Parameters
    ///
    /// - `request`: the request the plugin asked for.
    /// - `deadline`: the instant the whole call must be finished by, or `None`
    ///   when the caller is not running under a call deadline.
    ///
    /// # Returns
    ///
    /// The final response.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Denied`] when the allowlist or the address policy
    /// rejects the request or a redirect hop, [`HttpError::Timeout`] when the
    /// request outlives its budget or cannot get a rate-limit token in time,
    /// [`HttpError::BodyTooLarge`] when the response body exceeds the cap, and
    /// [`HttpError::Network`] for any other transport failure.
    pub async fn send(
        &self,
        request: &HttpRequest,
        deadline: Option<Instant>,
    ) -> Result<HttpResponse, HttpError> {
        let budget = Duration::from_millis(self.budget_ms(request.timeout_ms));
        let own_deadline = Instant::now() + budget;
        let deadline = Some(match deadline {
            Some(call) => call.min(own_deadline),
            None => own_deadline,
        });
        send_with_hops(
            &self.client,
            request,
            deadline,
            HopPolicy {
                grant: Some(&self.grant),
                pins: &self.pins,
                allow_anywhere: false,
                rate: Some(&self.rates),
            },
            self.grant.max_body_bytes,
        )
        .await
    }

    /// Returns the effective timeout budget in milliseconds.
    ///
    /// The plugin's request is clamped to `1..=MAX_TIMEOUT_MS`, defaulting to
    /// [`DEFAULT_TIMEOUT_MS`] when it asks for none.
    fn budget_ms(&self, requested: Option<u32>) -> u64 {
        u64::from(requested.unwrap_or(DEFAULT_TIMEOUT_MS as u32)).clamp(1, MAX_TIMEOUT_MS)
    }
}

/// What a hop loop applies besides the redirect rules.
///
/// The plugin's own client passes its grant, its pin map and its rate limiter;
/// the installer passes no grant and an "allow anywhere" flag when the index it
/// downloads from is a local one, so an archive URL is held to the same address
/// policy as the index it came from.
pub(crate) struct HopPolicy<'a> {
    /// The instance's grant, whose allowlist every hop is checked against, or
    /// `None` for a caller that fetches for the server itself.
    pub grant: Option<&'a HttpGrant>,
    /// The addresses the client's resolver hands to the connector, by host.
    pub pins: &'a Mutex<HashMap<String, Vec<SocketAddr>>>,
    /// Whether every host may resolve to non-public addresses.
    pub allow_anywhere: bool,
    /// The instance's rate limiters, when the request runs under a grant.
    pub rate: Option<&'a RateLimiters>,
}

/// Performs one request and its redirects under a policy.
///
/// Every hop is re-checked: the allowlist when the policy carries a grant, the
/// address policy always, and the rate limiter when there is one. A hop to
/// another origin drops the request's credential headers, and a redirect that
/// means "fetch this instead" (301, 302, 303) rewrites the method to GET without
/// the body, so a redirect cannot carry a plugin's API key to a second host.
///
/// # Parameters
///
/// - `client`: the HTTP client, built with a pinned resolver and
///   `redirect(Policy::none())` so this loop sees every hop.
/// - `request`: the request to send.
/// - `deadline`: the instant the whole request must be finished by.
/// - `policy`: the grant, pins, address exemption and rate limiter to apply.
/// - `max_body_bytes`: the response body cap.
///
/// # Returns
///
/// The final response.
///
/// # Errors
///
/// The same errors [`HttpClient::send`] documents.
pub(crate) async fn send_with_hops(
    client: &Client,
    request: &HttpRequest,
    deadline: Option<Instant>,
    policy: HopPolicy<'_>,
    max_body_bytes: u64,
) -> Result<HttpResponse, HttpError> {
    let mut current = request.clone();
    let mut url = Url::parse(&current.url)
        .map_err(|error| HttpError::Network(format!("invalid url: {error}")))?;

    let mut redirects = 0usize;
    loop {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(HttpError::Denied(format!(
                "scheme `{}` is not allowed; only http and https are",
                url.scheme()
            )));
        }
        let host = url
            .host_str()
            .ok_or_else(|| HttpError::Denied("the URL has no host".to_owned()))?
            .to_ascii_lowercase();
        if let Some(grant) = policy.grant
            && !grant.allows(&host)
        {
            return Err(HttpError::Denied(grant.denial(&host)));
        }

        let anywhere = policy.allow_anywhere
            || policy
                .grant
                .is_some_and(|grant| grant.may_resolve_anywhere(&host));
        let addresses = approved_addresses(&url, anywhere).await?;
        // A literal address is never resolved, so it needs no pin; only a name
        // is looked up by name by the connector.
        if literal_host(&url).is_none() {
            lock(policy.pins).insert(host.clone(), addresses);
        }

        if let Some(rates) = policy.rate
            && let Some(grant) = policy.grant
            && let Some(limiter) = rates.for_host(grant, &host)
        {
            wait_for_token(limiter, deadline).await?;
        }

        let method = Method::from_bytes(current.method.as_bytes())
            .map_err(|error| HttpError::Network(format!("invalid method: {error}")))?;
        let remaining = remaining_of(deadline)?;
        let mut builder = client.request(method, url.clone()).timeout(remaining);
        for (name, value) in &current.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &current.body {
            builder = builder.body(body.clone());
        }

        let response = builder.send().await.map_err(map_send_error)?;
        if !response.status().is_redirection() {
            return read_body(response, max_body_bytes).await;
        }
        if redirects >= MAX_REDIRECTS {
            return Err(HttpError::Denied(format!(
                "the request exceeded {MAX_REDIRECTS} redirects"
            )));
        }
        let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
            return read_body(response, max_body_bytes).await;
        };
        let location = location.to_str().map_err(|_| {
            HttpError::Network("the redirect location is not valid text".to_owned())
        })?;
        let next = url.join(location).map_err(|error| {
            HttpError::Network(format!("the redirect location is not a URL: {error}"))
        })?;
        if !matches!(next.scheme(), "http" | "https") {
            return Err(HttpError::Denied(format!(
                "the redirect target uses the `{}` scheme, which is not allowed",
                next.scheme()
            )));
        }
        current = next_request(&current, &url, &next, response.status().as_u16());
        url = next;
        redirects += 1;
    }
}

/// Builds the request one redirect hop sends.
///
/// A hop to another origin drops the request's credential headers
/// (`authorization`, `proxy-authorization`, `cookie`) and its body: both were
/// meant for the origin that issued the redirect, and a redirect is not a
/// reason to hand them to a second host. On such a hop a 301, 302 or 303 also
/// rewrites the method to GET, and a 307 or 308 keeps the method with no body.
/// A same-origin hop keeps the method and every header, which is what a plugin's
/// own API expects from its own redirects.
///
/// # Parameters
///
/// - `current`: the request that received the redirect.
/// - `from`: the URL that answered with it.
/// - `to`: the URL the redirect points at.
/// - `status`: the redirect's status code.
///
/// # Returns
///
/// The request to send next.
fn next_request(current: &HttpRequest, from: &Url, to: &Url, status: u16) -> HttpRequest {
    let mut next = current.clone();
    next.url = to.to_string();
    if same_origin(from, to) {
        return next;
    }
    next.headers.retain(|(name, _)| !is_credential_header(name));
    next.body = None;
    let method = next.method.to_ascii_uppercase();
    let keeps_method = method == "GET" || method == "HEAD";
    match status {
        303 => next.method = "GET".to_owned(),
        301 | 302 if !keeps_method => next.method = "GET".to_owned(),
        _ => {}
    }
    next
}

/// Returns whether two URLs have the same origin.
///
/// # Parameters
///
/// - `left`: one URL.
/// - `right`: the other URL.
///
/// # Returns
///
/// Whether the scheme, host and port match.
fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left
            .host_str()
            .map(str::to_ascii_lowercase)
            .eq(&right.host_str().map(str::to_ascii_lowercase))
        && left.port_or_known_default() == right.port_or_known_default()
}

/// Returns whether a header carries a credential.
///
/// # Parameters
///
/// - `name`: the header name, compared case-insensitively.
///
/// # Returns
///
/// Whether the header must not follow a redirect to another origin.
fn is_credential_header(name: &str) -> bool {
    ["authorization", "proxy-authorization", "cookie"]
        .iter()
        .any(|credential| name.eq_ignore_ascii_case(credential))
}

/// Returns the literal address a URL names, when its host is an IPv4 or IPv6
/// literal.
///
/// The `url` crate keeps the brackets around an IPv6 literal, and a bracketed
/// host cannot be resolved as a name, so the literal is taken apart here and
/// never sent to the resolver.
///
/// # Parameters
///
/// - `url`: the URL to read.
///
/// # Returns
///
/// The address, or `None` for a named host.
pub(crate) fn literal_host(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    let text = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    text.parse::<IpAddr>().ok()
}

/// Resolves one URL and returns the addresses the address policy accepts.
///
/// A literal address is used as it is - the URL names it, so nothing is
/// resolved - and an IPv6 literal keeps its brackets out of the address. A name
/// is resolved once, here, and the result is what the client's resolver hands to
/// the connector, so the policy is enforced on the addresses the connection uses.
///
/// # Parameters
///
/// - `url`: the URL to resolve.
/// - `allow_anywhere`: whether a non-public address is acceptable, which is the
///   case for a `$config:` host the administrator chose and for an archive a
///   local index points at.
///
/// # Returns
///
/// The accepted addresses, at least one.
///
/// # Errors
///
/// Returns [`HttpError::Denied`] when the URL has no host or resolves only to
/// non-public addresses, and [`HttpError::Network`] when the name cannot be
/// resolved.
pub(crate) async fn approved_addresses(
    url: &Url,
    allow_anywhere: bool,
) -> Result<Vec<SocketAddr>, HttpError> {
    let host = url
        .host_str()
        .ok_or_else(|| HttpError::Denied("the URL has no host".to_owned()))?
        .to_ascii_lowercase();
    let port = url.port_or_known_default().unwrap_or(80);

    let candidates: Vec<SocketAddr> = match literal_host(url) {
        Some(address) => vec![SocketAddr::new(address, port)],
        None => tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|error| HttpError::Network(format!("resolving `{host}` failed: {error}")))?
            .collect(),
    };

    let mut addresses = Vec::with_capacity(candidates.len());
    for address in candidates {
        if allow_anywhere || is_public(address.ip()) {
            addresses.push(address);
        } else {
            tracing::debug!(
                target: "hearthsound::plugin",
                host = %host,
                address = %address.ip(),
                "rejecting a non-public address",
            );
        }
    }
    if addresses.is_empty() {
        return Err(HttpError::Denied(format!(
            "host `{host}` resolves only to non-public addresses"
        )));
    }
    Ok(addresses)
}

/// Waits for a rate-limit token, giving up at the deadline.
async fn wait_for_token(
    limiter: &DefaultDirectRateLimiter,
    deadline: Option<Instant>,
) -> Result<(), HttpError> {
    match deadline {
        Some(deadline) => {
            let remaining = remaining_of(Some(deadline))?;
            match tokio::time::timeout(remaining, limiter.until_ready()).await {
                Ok(_) => Ok(()),
                Err(_) => Err(HttpError::Timeout),
            }
        }
        None => {
            limiter.until_ready().await;
            Ok(())
        }
    }
}

/// Reads a response into memory, enforcing a body cap while streaming.
///
/// # Errors
///
/// Returns [`HttpError::BodyTooLarge`] when the body exceeds `cap` and
/// [`HttpError::Network`] when a chunk cannot be read.
async fn read_body(mut response: reqwest::Response, cap: u64) -> Result<HttpResponse, HttpError> {
    let status = response.status().as_u16();
    let headers = header_pairs(response.headers());

    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() as u64 + chunk.len() as u64 > cap {
                    return Err(HttpError::BodyTooLarge(cap));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                if error.is_timeout() {
                    return Err(HttpError::Timeout);
                }
                return Err(HttpError::Network(error.to_string()));
            }
        }
    }

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Maps a transport error to the plugin's error vocabulary.
fn map_send_error(error: reqwest::Error) -> HttpError {
    if error.is_timeout() {
        HttpError::Timeout
    } else {
        HttpError::Network(error.to_string())
    }
}

/// Flattens response headers into lossy string pairs.
fn header_pairs(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// Returns the time left until `deadline`, failing when it has passed.
fn remaining_of(deadline: Option<Instant>) -> Result<Duration, HttpError> {
    let Some(deadline) = deadline else {
        return Err(HttpError::Network("no deadline was set".to_owned()));
    };
    let now = Instant::now();
    if now >= deadline {
        return Err(HttpError::Timeout);
    }
    Ok(deadline - now)
}

/// Resolves names from addresses the address policy already accepted.
///
/// The resolver refuses a name it was not given, so the HTTP client can never
/// resolve a host the policy never saw.
struct PinnedResolver {
    pins: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>>,
    slug: Option<Slug>,
}

impl Resolve for PinnedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let addresses = lock(&self.pins).get(&host).cloned();
        let slug = self.slug.clone();
        Box::pin(async move {
            match addresses {
                Some(addresses) if !addresses.is_empty() => {
                    Ok(Box::new(addresses.into_iter()) as Addrs)
                }
                _ => {
                    tracing::debug!(
                        target: "hearthsound::plugin",
                        instance = slug.as_ref().map(Slug::as_str).unwrap_or("-"),
                        host = %host,
                        "refusing to resolve a host the address policy did not approve",
                    );
                    Err(format!("host `{host}` was not resolved").into())
                }
            }
        })
    }
}

/// Builds an HTTP client whose resolver only hands out pinned addresses.
///
/// The client follows no redirect by itself, so the caller's hop loop sees every
/// hop and can re-check it. `slug` names the instance for the debug log; a
/// caller that fetches for the server itself - the installer - passes `None`.
///
/// # Parameters
///
/// - `pins`: the map [`approved_addresses`] populates, shared with the client.
/// - `user_agent`: the User-Agent the client sends.
/// - `slug`: the instance the client belongs to, when it belongs to one.
///
/// # Returns
///
/// The client.
///
/// # Errors
///
/// Returns [`HttpError::Network`] when the client cannot be built.
pub(crate) fn pinned_client(
    pins: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>>,
    user_agent: String,
    slug: Option<Slug>,
) -> Result<Client, HttpError> {
    let resolver = Arc::new(PinnedResolver { pins, slug });

    // reqwest is built without a bundled TLS provider; ring is installed as the
    // process default the first time a client is built. Installing fails
    // harmlessly when a default is already in place.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Client::builder()
        .user_agent(user_agent)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(resolver)
        .build()
        .map_err(|error| HttpError::Network(error.to_string()))
}

/// Returns whether `ip` is a public address the address policy accepts.
///
/// The rejected ranges are the non-routable, internal and documentation
/// ranges: private, loopback, link-local, carrier-grade NAT, multicast,
/// unspecified, broadcast, benchmarking, reserved and documentation (including
/// the `192.0.0.0/24` block), unique-local and IPv6 link-local, every IPv4
/// address mapped into IPv6, and the IPv6 ranges that carry an IPv4 address or
/// a private scope inside them - IPv4-compatible `::/96`, NAT64 `64:ff9b::/96`,
/// 6to4 `2002::/16` and site-local `fec0::/10`.
///
/// Nothing checks the address again at connect time because there is no second
/// check to make: [`approved_addresses`] is the only place a name becomes an
/// address, and the client's resolver only hands the connector the addresses it
/// returned, so the pin is the connect-time enforcement.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(mapped) => is_public_v4(mapped),
            None => is_public_v6(address),
        },
    }
}

/// Returns whether an IPv4 address is public.
///
/// The standard library's `is_shared`, `is_benchmarking` and `is_reserved` are
/// still unstable, so the carrier-grade NAT, benchmarking and reserved blocks
/// are matched by their leading octets.
fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _d] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_multicast()
        || address.is_unspecified()
        || address.is_documentation()
        || a == 0
        || a >= 240
        || (a == 192 && b == 0 && c == 0)
        || (a == 100 && (64..=127).contains(&b))
        || (a == 198 && (18..=19).contains(&b)))
}

/// Returns whether an IPv6 address is public.
///
/// Besides the standard library's own predicates, four ranges are matched by
/// their leading segments: IPv4-compatible `::/96` (which also covers
/// `::5efe:a.b.c.d`, an address `to_ipv4_mapped` does not map), NAT64
/// `64:ff9b::/96`, 6to4 `2002::/16` and site-local `fec0::/10`.
fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || segments[..6] == [0; 6]
        || (segments[0] == 0x0064 && segments[1] == 0xff9b)
        || segments[0] == 0x2002
        || (segments[0] & 0xffc0) == 0xfec0)
}

/// Locks a mutex, ignoring poisoning.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::HostEntry;

    fn grant(hosts: &[&str], config: &str) -> HttpGrant {
        let permission = HttpPermission {
            hosts: hosts
                .iter()
                .map(|host| HostEntry::Pattern((*host).to_owned()))
                .collect(),
            rate_limit: None,
            max_body_bytes: None,
        };
        HttpGrant::new(Some(&permission), config)
    }

    #[test]
    fn accepts_literal_and_wildcard_hosts() {
        let grant = grant(&["api.example.com", "*.example.org"], "{}");
        assert!(grant.allows("api.example.com"));
        assert!(!grant.allows("other.example.com"));
        assert!(grant.allows("a.example.org"));
        assert!(grant.allows("a.b.example.org"));
        assert!(!grant.allows("example.org"));
        assert!(!grant.allows("notexample.org"));
    }

    #[test]
    fn resolves_config_hosts_from_the_server_config() {
        let grant = grant(
            &["$config:url"],
            r#"{"url":"http://slskd.internal:5030/api"}"#,
        );
        assert!(grant.allows("slskd.internal"));
        assert!(grant.may_resolve_anywhere("slskd.internal"));
        assert!(!grant.allows("other.internal"));
    }

    #[test]
    fn an_absent_config_field_matches_nothing() {
        let grant = grant(&["$config:url"], "{}");
        assert!(!grant.allows("slskd.internal"));
        assert!(grant.denial("slskd.internal").contains("unset"));
    }

    #[test]
    fn rejects_non_public_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "224.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "192.0.2.5",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "::7f00:1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
            "fec0::1",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(!is_public(ip), "{address} must be rejected");
        }
    }

    #[test]
    fn accepts_public_addresses() {
        for address in [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700::1111",
            "2a00:1450:4001:81b::200e",
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(is_public(ip), "{address} must be accepted");
        }
    }

    #[tokio::test]
    async fn an_ipv6_literal_is_pinned_without_a_lookup() {
        let public = Url::parse("https://[2606:4700::1111]/x").unwrap();
        let addresses = approved_addresses(&public, false).await.unwrap();
        assert_eq!(
            addresses,
            vec!["[2606:4700::1111]:443".parse::<SocketAddr>().unwrap()]
        );

        // A documentation-range literal is denied by the address policy, not by
        // a failed lookup.
        let denied = Url::parse("https://[2001:db8::1]/x").unwrap();
        let error = approved_addresses(&denied, false).await.unwrap_err();
        assert!(
            matches!(&error, HttpError::Denied(reason) if reason.contains("non-public")),
            "{error}"
        );

        // A `$config:` host may name a non-public address.
        assert!(approved_addresses(&denied, true).await.is_ok());
    }

    #[test]
    fn a_cross_origin_redirect_drops_the_credential() {
        let from = Url::parse("https://api.example.com/search").unwrap();
        let to = Url::parse("https://other.example.org/collect").unwrap();
        let request = HttpRequest {
            method: "POST".to_owned(),
            url: from.to_string(),
            headers: vec![
                ("Authorization".to_owned(), "Bearer key".to_owned()),
                ("Cookie".to_owned(), "session=1".to_owned()),
                ("X-Request-Id".to_owned(), "1".to_owned()),
            ],
            body: Some(b"{\"q\":\"x\"}".to_vec()),
            timeout_ms: None,
        };

        let next = next_request(&request, &from, &to, 302);
        assert_eq!(next.method, "GET");
        assert_eq!(next.url, to.to_string());
        assert!(next.body.is_none());
        assert_eq!(
            next.headers,
            vec![("X-Request-Id".to_owned(), "1".to_owned())]
        );

        // A 307 keeps the method, and the body still goes: it was meant for the
        // origin that issued the redirect.
        let next = next_request(&request, &from, &to, 307);
        assert_eq!(next.method, "POST");
        assert!(next.body.is_none());
        assert_eq!(next.headers.len(), 1);

        // A 301 on a GET only drops the credentials.
        let mut get = request.clone();
        get.method = "GET".to_owned();
        let next = next_request(&get, &from, &to, 301);
        assert_eq!(next.method, "GET");
        assert_eq!(next.headers.len(), 1);
    }

    #[test]
    fn a_same_origin_redirect_is_unchanged() {
        let from = Url::parse("https://api.example.com/search").unwrap();
        let to = Url::parse("https://api.example.com/v2/search").unwrap();
        let request = HttpRequest {
            method: "POST".to_owned(),
            url: from.to_string(),
            headers: vec![
                ("Authorization".to_owned(), "Bearer key".to_owned()),
                ("Cookie".to_owned(), "session=1".to_owned()),
            ],
            body: Some(b"{\"q\":\"x\"}".to_vec()),
            timeout_ms: None,
        };

        let next = next_request(&request, &from, &to, 302);
        assert_eq!(next.method, "POST");
        assert_eq!(next.body, request.body);
        assert_eq!(next.headers, request.headers);
        assert_eq!(next.url, to.to_string());
    }

    #[test]
    fn clamps_the_body_cap() {
        let permission = HttpPermission {
            hosts: vec![HostEntry::Pattern("example.com".to_owned())],
            rate_limit: None,
            max_body_bytes: Some(u64::MAX),
        };
        assert_eq!(
            HttpGrant::new(Some(&permission), "{}").max_body_bytes,
            MAX_BODY_BYTES
        );
        let permission = HttpPermission {
            hosts: vec![HostEntry::Pattern("example.com".to_owned())],
            rate_limit: None,
            max_body_bytes: None,
        };
        assert_eq!(
            HttpGrant::new(Some(&permission), "{}").max_body_bytes,
            DEFAULT_BODY_BYTES
        );
    }
}

/// Enforcement tests that drive the `host-http` import against a local HTTP
/// server, so every rule is exercised through the real import implementation.
#[cfg(test)]
mod enforcement {
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::api::v4::bindings::hearthsound::plugin::host_http;
    use crate::files::FileAccess;
    use crate::kv::KvStore;
    use crate::log::LogSink;
    use crate::manifest::HostEntry;
    use crate::state::{InstanceServices, InstanceState};

    /// Builds the state of one instance, as the pool would.
    fn state(server_config: &str, permission: Option<HttpPermission>) -> InstanceState {
        let slug = Slug::new("probe").unwrap();
        let grant = HttpGrant::new(permission.as_ref(), server_config);
        let http = Arc::new(
            HttpClient::new("com.example.probe".to_owned(), "0.1.0", slug.clone(), grant).unwrap(),
        );
        let log = Arc::new(LogSink::new("com.example.probe".to_owned(), slug.clone()));
        let kv = Arc::new(KvStore::new(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
            KvStore::new_instance_id(),
            crate::kv::DEFAULT_QUOTA_BYTES,
        ));
        let files = Arc::new(FileAccess::default());
        InstanceState::new(
            slug.clone(),
            Arc::from(server_config),
            log,
            64,
            InstanceServices {
                http,
                kv,
                files,
                process: Arc::new(crate::process::ProcessTable::without_tools(
                    "com.example.probe",
                    &slug,
                )),
            },
        )
    }

    /// The permission that reaches a local test server through `$config:baseUrl`.
    fn config_permission(max_body_bytes: Option<u64>) -> HttpPermission {
        HttpPermission {
            hosts: vec![HostEntry::Pattern("$config:baseUrl".to_owned())],
            rate_limit: None,
            max_body_bytes,
        }
    }

    /// Calls `host-http.send` once and returns the status and body.
    async fn get(
        state: &mut InstanceState,
        url: &str,
    ) -> Result<(u16, Vec<u8>), host_http::HttpError> {
        let request = host_http::Request {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            body: None,
            timeout_ms: None,
        };
        match host_http::Host::send(state, request).await.unwrap() {
            Ok(response) => Ok((response.status, response.body)),
            Err(error) => Err(error),
        }
    }

    #[tokio::test]
    async fn sends_to_a_config_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hello"))
            .respond_with(ResponseTemplate::new(200).set_body_string("world"))
            .expect(1)
            .mount(&server)
            .await;

        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(config_permission(None)));
        let (status, body) = get(&mut state, &format!("{}/hello", server.uri()))
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"world");
    }

    #[tokio::test]
    async fn denies_a_literal_host_that_resolves_to_loopback() {
        let permission = HttpPermission {
            hosts: vec![HostEntry::Pattern("localhost".to_owned())],
            rate_limit: None,
            max_body_bytes: None,
        };
        let mut state = state("{}", Some(permission));
        let error = get(&mut state, "http://localhost:9/hello")
            .await
            .unwrap_err();
        assert!(
            matches!(&error, host_http::HttpError::Denied(reason) if reason.contains("non-public")),
            "expected a non-public denial, got {error:?}"
        );
    }

    #[tokio::test]
    async fn denies_a_host_outside_the_allowlist() {
        let server = MockServer::start().await;
        let permission = HttpPermission {
            hosts: vec![HostEntry::Pattern("api.example.com".to_owned())],
            rate_limit: None,
            max_body_bytes: None,
        };
        let mut state = state("{}", Some(permission));
        let error = get(&mut state, &format!("{}/hello", server.uri()))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, host_http::HttpError::Denied(reason) if reason.contains("allowlist")),
            "expected an allowlist denial, got {error:?}"
        );
    }

    #[tokio::test]
    async fn denies_everything_without_an_http_permission() {
        let server = MockServer::start().await;
        let mut state = state("{}", None);
        let error = get(&mut state, &format!("{}/hello", server.uri()))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, host_http::HttpError::Denied(reason) if reason.contains("not approved")),
            "expected a blanket denial, got {error:?}"
        );
    }

    #[tokio::test]
    async fn denies_a_redirect_off_the_allowlist() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "http://example.com/elsewhere"),
            )
            .mount(&server)
            .await;

        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(config_permission(None)));
        let error = get(&mut state, &format!("{}/start", server.uri()))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, host_http::HttpError::Denied(reason) if reason.contains("example.com")),
            "expected a redirect denial, got {error:?}"
        );
    }

    #[tokio::test]
    async fn follows_a_redirect_inside_the_allowlist() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/end"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/end"))
            .respond_with(ResponseTemplate::new(200).set_body_string("arrived"))
            .expect(1)
            .mount(&server)
            .await;

        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(config_permission(None)));
        let (status, body) = get(&mut state, &format!("{}/start", server.uri()))
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"arrived");
    }

    #[tokio::test]
    async fn a_cross_origin_redirect_does_not_forward_the_authorization_header() {
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/end", second.uri())),
            )
            .mount(&first)
            .await;
        Mock::given(method("GET"))
            .and(path("/end"))
            .respond_with(ResponseTemplate::new(200).set_body_string("arrived"))
            .mount(&second)
            .await;

        // Both servers are on `127.0.0.1`, which the `$config` host allows, so
        // the hop is the only thing that decides whether the credential travels.
        let config = format!(r#"{{"baseUrl":"{}"}}"#, first.uri());
        let mut state = state(&config, Some(config_permission(None)));
        let request = host_http::Request {
            method: "GET".to_owned(),
            url: format!("{}/start", first.uri()),
            headers: vec![("authorization".to_owned(), "Bearer secret".to_owned())],
            body: None,
            timeout_ms: None,
        };
        let status = match host_http::Host::send(&mut state, request).await.unwrap() {
            Ok(response) => response.status,
            Err(error) => panic!("the redirect should have been followed: {error:?}"),
        };
        assert_eq!(status, 200);

        let requests = second.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].headers.get("authorization").is_none(),
            "the credential followed a redirect to another origin"
        );
        let requests = first.received_requests().await.unwrap();
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .map(|value| value.to_str().unwrap()),
            Some("Bearer secret")
        );
    }

    #[tokio::test]
    async fn a_same_origin_redirect_keeps_the_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/end"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/end"))
            .respond_with(ResponseTemplate::new(200).set_body_string("arrived"))
            .mount(&server)
            .await;

        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(config_permission(None)));
        let request = host_http::Request {
            method: "GET".to_owned(),
            url: format!("{}/start", server.uri()),
            headers: vec![("authorization".to_owned(), "Bearer secret".to_owned())],
            body: None,
            timeout_ms: None,
        };
        match host_http::Host::send(&mut state, request).await.unwrap() {
            Ok(response) => assert_eq!(response.status, 200),
            Err(error) => panic!("the redirect should have been followed: {error:?}"),
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert_eq!(
                request
                    .headers
                    .get("authorization")
                    .map(|value| value.to_str().unwrap()),
                Some("Bearer secret")
            );
        }
    }

    #[tokio::test]
    async fn enforces_the_body_cap_while_streaming() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 4096]))
            .mount(&server)
            .await;

        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(config_permission(Some(64))));
        let error = get(&mut state, &format!("{}/big", server.uri()))
            .await
            .unwrap_err();
        assert!(
            matches!(error, host_http::HttpError::BodyTooLarge),
            "expected a body-too-large error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn times_out_a_slow_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(750)))
            .mount(&server)
            .await;

        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(config_permission(None)));
        let request = host_http::Request {
            method: "GET".to_owned(),
            url: format!("{}/slow", server.uri()),
            headers: Vec::new(),
            body: None,
            timeout_ms: Some(50),
        };
        let outcome = host_http::Host::send(&mut state, request).await.unwrap();
        let error = outcome.unwrap_err();
        assert!(
            matches!(error, host_http::HttpError::Timeout),
            "expected a timeout, got {error:?}"
        );
    }

    #[tokio::test]
    async fn rate_limits_requests() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tick"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let permission = HttpPermission {
            hosts: vec![HostEntry::Pattern("$config:baseUrl".to_owned())],
            rate_limit: Some(crate::manifest::RateLimit {
                per_second: 1,
                burst: 1,
            }),
            max_body_bytes: None,
        };
        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(permission));

        let started = Instant::now();
        get(&mut state, &format!("{}/tick", server.uri()))
            .await
            .unwrap();
        get(&mut state, &format!("{}/tick", server.uri()))
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "the second request should have waited a token, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_hosts_own_rate_limit_does_not_throttle_another_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tick"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let permission = HttpPermission {
            hosts: vec![
                HostEntry::Limited {
                    host: "$config:slowUrl".to_owned(),
                    rate_limit: crate::manifest::RateLimit {
                        per_second: 1,
                        burst: 1,
                    },
                },
                HostEntry::Pattern("$config:fastUrl".to_owned()),
            ],
            rate_limit: None,
            max_body_bytes: None,
        };
        // Two names for the same test server, so each request matches its own
        // allowlist rule instead of the first rule that names the host.
        let slow = server.uri();
        let fast = format!("http://localhost:{}/", server.address().port());
        let config = format!(r#"{{"slowUrl":"{slow}","fastUrl":"{fast}"}}"#);
        let mut state = state(&config, Some(permission));

        let throttled = Instant::now();
        get(&mut state, &format!("{slow}/tick")).await.unwrap();
        get(&mut state, &format!("{slow}/tick")).await.unwrap();
        assert!(
            throttled.elapsed() >= Duration::from_millis(900),
            "the limited host should have waited a token, took {:?}",
            throttled.elapsed()
        );

        // The unlimited host shares no limiter with the limited one, and the
        // instance declares none, so neither request waits.
        let free = Instant::now();
        get(&mut state, &format!("{fast}tick")).await.unwrap();
        get(&mut state, &format!("{fast}tick")).await.unwrap();
        assert!(
            free.elapsed() < Duration::from_millis(500),
            "the unlimited host should not wait, took {:?}",
            free.elapsed()
        );
    }

    #[tokio::test]
    async fn rate_limit_wait_does_not_outlive_the_call_deadline() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tick"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let permission = HttpPermission {
            hosts: vec![HostEntry::Pattern("$config:baseUrl".to_owned())],
            rate_limit: Some(crate::manifest::RateLimit {
                per_second: 1,
                burst: 1,
            }),
            max_body_bytes: None,
        };
        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(permission));

        get(&mut state, &format!("{}/tick", server.uri()))
            .await
            .unwrap();
        // The next request has to wait almost a second for a token; a call with
        // 50 ms left must fail instead of waiting it out.
        state.begin_call(Instant::now() + Duration::from_millis(50));
        let error = get(&mut state, &format!("{}/tick", server.uri()))
            .await
            .unwrap_err();
        assert!(
            matches!(error, host_http::HttpError::Timeout),
            "expected a timeout, got {error:?}"
        );
    }

    #[tokio::test]
    async fn adds_a_user_agent_and_keeps_the_plugins_own() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ua"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(config_permission(None)));
        get(&mut state, &format!("{}/ua", server.uri()))
            .await
            .unwrap();

        let request = host_http::Request {
            method: "GET".to_owned(),
            url: format!("{}/ua", server.uri()),
            headers: vec![("user-agent".to_owned(), "my-client/1".to_owned())],
            body: None,
            timeout_ms: None,
        };
        host_http::Host::send(&mut state, request)
            .await
            .unwrap()
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let default = requests[0]
            .headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(default.starts_with("Hearthsound/"), "got {default}");
        assert!(
            default.contains("plugin/com.example.probe/0.1.0"),
            "got {default}"
        );
        let own = requests[1]
            .headers
            .get("user-agent")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(own, "my-client/1");
    }

    #[tokio::test]
    async fn clamps_a_long_plugin_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/quick"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let config = format!(r#"{{"baseUrl":"{}"}}"#, server.uri());
        let mut state = state(&config, Some(config_permission(None)));
        let request = host_http::Request {
            method: "GET".to_owned(),
            url: format!("{}/quick", server.uri()),
            headers: Vec::new(),
            body: None,
            timeout_ms: Some(u32::MAX),
        };
        // A generous deadline must not turn the clamp into a short timeout.
        state.begin_call(Instant::now() + Duration::from_secs(30));
        let (status, _) = match host_http::Host::send(&mut state, request).await.unwrap() {
            Ok(response) => (response.status, response.body),
            Err(error) => panic!("expected a response, got {error:?}"),
        };
        assert_eq!(status, 200);
    }
}
