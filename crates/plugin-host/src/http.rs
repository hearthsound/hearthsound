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
//!    and the address policy.
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
                rate_limit: None,
                max_body_bytes: DEFAULT_BODY_BYTES,
            };
        };

        let config: serde_json::Value =
            serde_json::from_str(server_config).unwrap_or(serde_json::Value::Null);
        let rules = permission
            .hosts
            .iter()
            .map(|host| match host.strip_prefix("$config:") {
                Some(field) => HostRule::Config {
                    field: field.to_owned(),
                    host: config_host(&config, field),
                },
                None => match host.strip_prefix("*.") {
                    Some(domain) => HostRule::Wildcard(domain.to_ascii_lowercase()),
                    None => HostRule::Literal(host.to_ascii_lowercase()),
                },
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
            rate_limit,
            max_body_bytes,
        }
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
    plugin: String,
    slug: Slug,
    grant: HttpGrant,
    client: Client,
    rate: Option<DefaultDirectRateLimiter>,
    pins: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>>,
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
        let resolver = Arc::new(PinnedResolver {
            pins: Arc::clone(&pins),
            slug: slug.clone(),
        });

        let user_agent = format!(
            "Hearthsound/{} (+https://hearthsound.app) plugin/{}/{}",
            env!("CARGO_PKG_VERSION"),
            plugin,
            plugin_version
        );

        let client = Client::builder()
            .user_agent(user_agent)
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(resolver)
            .build()
            .map_err(|error| HttpError::Network(error.to_string()))?;

        let rate = grant.rate_limit.map(|(per_second, burst)| {
            RateLimiter::direct(Quota::per_second(per_second).allow_burst(burst))
        });

        Ok(HttpClient {
            plugin,
            slug,
            grant,
            client,
            rate,
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
        let method = Method::from_bytes(request.method.as_bytes())
            .map_err(|error| HttpError::Network(format!("invalid method: {error}")))?;
        let mut url = Url::parse(&request.url)
            .map_err(|error| HttpError::Network(format!("invalid url: {error}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(HttpError::Denied(format!(
                "scheme `{}` is not allowed; only http and https are",
                url.scheme()
            )));
        }

        let budget = Duration::from_millis(self.budget_ms(request.timeout_ms));
        let own_deadline = Instant::now() + budget;
        let deadline = Some(match deadline {
            Some(call) => call.min(own_deadline),
            None => own_deadline,
        });

        let mut redirects = 0usize;
        loop {
            self.check_allowlist(&url)?;
            self.pin(&url).await?;
            self.acquire_token(deadline).await?;

            let remaining = remaining_of(deadline)?;
            let mut builder = self
                .client
                .request(method.clone(), url.clone())
                .timeout(remaining);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = &request.body {
                builder = builder.body(body.clone());
            }

            let response = builder.send().await.map_err(map_send_error)?;
            if response.status().is_redirection() {
                if redirects >= MAX_REDIRECTS {
                    return Err(HttpError::Denied(format!(
                        "the request exceeded {MAX_REDIRECTS} redirects"
                    )));
                }
                let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                    return self.read_body(response).await;
                };
                let location = location.to_str().map_err(|_| {
                    HttpError::Network("the redirect location is not valid text".to_owned())
                })?;
                url = url.join(location).map_err(|error| {
                    HttpError::Network(format!("the redirect location is not a URL: {error}"))
                })?;
                if !matches!(url.scheme(), "http" | "https") {
                    return Err(HttpError::Denied(format!(
                        "the redirect target uses the `{}` scheme, which is not allowed",
                        url.scheme()
                    )));
                }
                redirects += 1;
                continue;
            }

            return self.read_body(response).await;
        }
    }

    /// Returns the effective timeout budget in milliseconds.
    ///
    /// The plugin's request is clamped to `1..=MAX_TIMEOUT_MS`, defaulting to
    /// [`DEFAULT_TIMEOUT_MS`] when it asks for none.
    fn budget_ms(&self, requested: Option<u32>) -> u64 {
        u64::from(requested.unwrap_or(DEFAULT_TIMEOUT_MS as u32)).clamp(1, MAX_TIMEOUT_MS)
    }

    /// Checks the request's host against the allowlist.
    fn check_allowlist(&self, url: &Url) -> Result<(), HttpError> {
        let host = url
            .host_str()
            .ok_or_else(|| HttpError::Denied("the URL has no host".to_owned()))?
            .to_ascii_lowercase();
        if self.grant.allows(&host) {
            Ok(())
        } else {
            Err(HttpError::Denied(self.grant.denial(&host)))
        }
    }

    /// Resolves the request's host and pins the accepted addresses.
    ///
    /// The pin is what the HTTP client's resolver hands to the connector, so the
    /// address policy is enforced on the addresses the connection uses and the
    /// name is never resolved a second time.
    async fn pin(&self, url: &Url) -> Result<(), HttpError> {
        let host = url
            .host_str()
            .ok_or_else(|| HttpError::Denied("the URL has no host".to_owned()))?
            .to_ascii_lowercase();
        let port = url.port_or_known_default().unwrap_or(80);
        let anywhere = self.grant.may_resolve_anywhere(&host);

        let resolved = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|error| HttpError::Network(format!("resolving `{host}` failed: {error}")))?;

        let mut addresses = Vec::new();
        for address in resolved {
            if anywhere || is_public(address.ip()) {
                addresses.push(address);
            } else {
                tracing::debug!(
                    target: "hearthsound::plugin",
                    plugin = %self.plugin,
                    instance = %self.slug,
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

        lock(&self.pins).insert(host, addresses);
        Ok(())
    }

    /// Waits for a rate-limit token, giving up at the deadline.
    async fn acquire_token(&self, deadline: Option<Instant>) -> Result<(), HttpError> {
        let Some(limiter) = &self.rate else {
            return Ok(());
        };
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

    /// Reads a response into memory, enforcing the body cap while streaming.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::BodyTooLarge`] when the body exceeds the instance's
    /// cap and [`HttpError::Network`] when a chunk cannot be read.
    async fn read_body(&self, mut response: reqwest::Response) -> Result<HttpResponse, HttpError> {
        let status = response.status().as_u16();
        let headers = header_pairs(response.headers());
        let cap = self.grant.max_body_bytes;

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
    slug: Slug,
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
                        instance = %slug,
                        host = %host,
                        "refusing to resolve a host the address policy did not approve",
                    );
                    Err(format!("host `{host}` was not resolved").into())
                }
            }
        })
    }
}

/// Returns whether `ip` is a public address the address policy accepts.
///
/// The rejected ranges are the non-routable, internal and documentation
/// ranges: private, loopback, link-local, carrier-grade NAT, multicast,
/// unspecified, broadcast, benchmarking, reserved and documentation (including
/// the `192.0.0.0/24` block), unique-local and IPv6 link-local, plus every IPv4
/// address mapped into IPv6.
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
fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
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

    fn grant(hosts: &[&str], config: &str) -> HttpGrant {
        let permission = HttpPermission {
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
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
        ] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(!is_public(ip), "{address} must be rejected");
        }
    }

    #[test]
    fn accepts_public_addresses() {
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(is_public(ip), "{address} must be accepted");
        }
    }

    #[test]
    fn clamps_the_body_cap() {
        let permission = HttpPermission {
            hosts: vec!["example.com".to_owned()],
            rate_limit: None,
            max_body_bytes: Some(u64::MAX),
        };
        assert_eq!(
            HttpGrant::new(Some(&permission), "{}").max_body_bytes,
            MAX_BODY_BYTES
        );
        let permission = HttpPermission {
            hosts: vec!["example.com".to_owned()],
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
    use crate::bindings::hearthsound::plugin::host_http;
    use crate::log::LogSink;
    use crate::state::InstanceState;

    /// Builds the state of one instance, as the pool would.
    fn state(server_config: &str, permission: Option<HttpPermission>) -> InstanceState {
        let slug = Slug::new("probe").unwrap();
        let grant = HttpGrant::new(permission.as_ref(), server_config);
        let http = Arc::new(
            HttpClient::new("com.example.probe".to_owned(), "0.1.0", slug.clone(), grant).unwrap(),
        );
        let log = Arc::new(LogSink::new("com.example.probe".to_owned(), slug.clone()));
        InstanceState::new(slug, Arc::from(server_config), log, 64, http)
    }

    /// The permission that reaches a local test server through `$config:baseUrl`.
    fn config_permission(max_body_bytes: Option<u64>) -> HttpPermission {
        HttpPermission {
            hosts: vec!["$config:baseUrl".to_owned()],
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
            hosts: vec!["localhost".to_owned()],
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
            hosts: vec!["api.example.com".to_owned()],
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
            hosts: vec!["$config:baseUrl".to_owned()],
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
    async fn rate_limit_wait_does_not_outlive_the_call_deadline() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tick"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let permission = HttpPermission {
            hosts: vec!["$config:baseUrl".to_owned()],
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
