//! The parameters of a `/rest` call.
//!
//! Parameters arrive in the query string, in the body of an
//! `application/x-www-form-urlencoded` POST (the `formPost` extension), or in
//! both. [`Params`] reads the query string first and then the form body, and a
//! lookup returns the first match, so a query parameter wins over a body
//! parameter of the same name. A repeated name keeps every value in request
//! order, which is what the endpoints that take a list of ids need.

use axum::body::{Body, to_bytes};
use axum::extract::{FromRequest, Request};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response as HttpResponse};
use hearthsound_auth::SubsonicCredentials;

use crate::response::{Encoded, Response, ResponseFormat};

/// Returns the current time in unix milliseconds.
///
/// # Returns
///
/// The instant, from the system clock.
pub(crate) fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    i64::try_from(nanos / 1_000_000).unwrap_or(0)
}

/// The largest `application/x-www-form-urlencoded` body this server reads.
pub const MAX_FORM_BODY_BYTES: usize = 1024 * 1024;

/// The parameter that opts a request into the vendor `hearthsound` extension.
const VENDOR_PARAMETER: &str = "hearthsound";

/// The value of [`VENDOR_PARAMETER`] that enables it.
const VENDOR_VALUE: &str = "1";

/// The parameters of one `/rest` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    /// The parameter values, in request order, query string before form body.
    values: Vec<(String, String)>,
    /// The encoding the request asked for.
    format: ResponseFormat,
}

impl Params {
    /// Collects parameter values and reads the requested encoding from them.
    ///
    /// # Parameters
    ///
    /// - `values`: the parameter values, query string first.
    ///
    /// # Returns
    ///
    /// The parameters, with the format the `f` parameter selected.
    pub(crate) fn from_values(values: Vec<(String, String)>) -> Params {
        let format = ResponseFormat::from_parameter(
            values
                .iter()
                .find(|(name, _)| name == "f")
                .map(|(_, value)| value.as_str()),
        );
        Params { values, format }
    }

    /// Returns the first value of a parameter.
    ///
    /// # Parameters
    ///
    /// - `name`: the parameter name, as the client wrote it.
    ///
    /// # Returns
    ///
    /// The value, or `None` when the request did not carry the parameter.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Returns every value of a parameter, in request order.
    ///
    /// # Parameters
    ///
    /// - `name`: the parameter name.
    ///
    /// # Returns
    ///
    /// An iterator over the values; empty when the request did not carry the
    /// parameter.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.values
            .iter()
            .filter(move |(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Reports whether the request carried a parameter, with any value.
    ///
    /// # Parameters
    ///
    /// - `name`: the parameter name.
    pub fn has(&self, name: &str) -> bool {
        self.values.iter().any(|(key, _)| key == name)
    }

    /// Reports whether some value of a parameter is `value`.
    ///
    /// # Parameters
    ///
    /// - `name`: the parameter name.
    /// - `value`: the value to look for.
    pub fn matches(&self, name: &str, value: &str) -> bool {
        self.get_all(name).any(|candidate| candidate == value)
    }

    /// Returns every parameter, in request order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Returns the number of values the request carried, counting a repeated
    /// name once per value.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Reports whether the request carried no parameters at all.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns the encoding the request asked for.
    ///
    /// See [`ResponseFormat::from_parameter`] for how `f` selects it.
    pub fn format(&self) -> ResponseFormat {
        self.format
    }

    /// Reports whether the client opted into the vendor `hearthsound`
    /// extension.
    ///
    /// # Returns
    ///
    /// `true` when the request carried `hearthsound=1`. Vendor fields and
    /// vendor endpoints must be answered only when this is `true`, because a
    /// standard response is otherwise byte-identical to one from a server that
    /// has no vendor extension at all.
    pub fn vendor_enabled(&self) -> bool {
        self.matches(VENDOR_PARAMETER, VENDOR_VALUE)
    }

    /// Returns the client's name, the `c` parameter.
    ///
    /// The value is informational: clients send it so a server can tell a
    /// client's requests apart, and some clients adjust their own behavior to
    /// it. It is not part of the credential check.
    pub fn client_name(&self) -> Option<&str> {
        self.get("c")
    }

    /// Returns the protocol version the client implements, the `v` parameter.
    ///
    /// The value is informational: the server answers a
    /// supported protocol version in the envelope and does not negotiate on the
    /// client's, so no call is rejected for an older or newer `v`.
    pub fn protocol_version(&self) -> Option<&str> {
        self.get("v")
    }

    /// Returns the credentials the request presented.
    ///
    /// # Returns
    ///
    /// The `u`, `p`, `t`, `s`, and `apiKey` parameters, each `None` when the
    /// request did not carry it. Contradictory combinations are the credential
    /// check's business, not this one's.
    pub fn credentials(&self) -> SubsonicCredentials {
        SubsonicCredentials {
            username: self.get("u").map(str::to_owned),
            password: self.get("p").map(str::to_owned),
            token: self.get("t").map(str::to_owned),
            salt: self.get("s").map(str::to_owned),
            api_key: self.get("apiKey").map(str::to_owned),
        }
    }

    /// Renders a response in the encoding the client asked for.
    ///
    /// # Parameters
    ///
    /// - `response`: the response to render.
    ///
    /// # Returns
    ///
    /// The encoded response, ready to be returned from a handler.
    pub fn encode(&self, response: Response) -> Encoded {
        response.encode(self.format)
    }
}

/// A request whose parameters could not be read.
///
/// This is not a Subsonic error: a request that is not a well-formed HTTP form
/// post has no envelope to put an error in, so it is answered with a bare HTTP
/// status code.
#[derive(Debug, thiserror::Error)]
pub enum ParamsRejection {
    /// The form body declares more bytes than the cap allows.
    #[error("the form body is larger than {MAX_FORM_BODY_BYTES} bytes")]
    BodyTooLarge,
    /// The form body could not be read, for example because it exceeded the cap
    /// while it was being read.
    #[error("the request body could not be read")]
    Unreadable,
    /// A body declared as JSON is not a JSON document.
    #[error("the request body is not a JSON document")]
    InvalidJson,
}

impl IntoResponse for ParamsRejection {
    /// Answers the request with the HTTP status that describes the malformed
    /// body, and an empty body.
    fn into_response(self) -> HttpResponse {
        let status = match self {
            ParamsRejection::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ParamsRejection::Unreadable | ParamsRejection::InvalidJson => StatusCode::BAD_REQUEST,
        };
        tracing::debug!(status = status.as_u16(), reason = %self, "rejected a /rest request");
        status.into_response()
    }
}

impl<S: Send + Sync> FromRequest<S> for Params {
    type Rejection = ParamsRejection;

    /// Parses a request's query string and form body into [`Params`].
    ///
    /// # Errors
    ///
    /// Returns a [`ParamsRejection`] when a form body cannot be read, which the
    /// caller answers with a bare HTTP status code.
    async fn from_request(request: Request, _state: &S) -> Result<Params, ParamsRejection> {
        let (parts, body) = request.into_parts();
        let values = read_values(&parts, body).await?;
        Ok(Params::from_values(values))
    }
}

/// Reads the parameter values of a request.
///
/// The query string is read first, then the body of a POST whose content type is
/// `application/x-www-form-urlencoded` (or missing, which clients that send the
/// form post extension's body without a content type rely on).
///
/// # Parameters
///
/// - `parts`: the request's head.
/// - `body`: the request's body, which is read only for a form post.
///
/// # Errors
///
/// Returns [`ParamsRejection::BodyTooLarge`] when the form body declares more
/// bytes than [`MAX_FORM_BODY_BYTES`], and [`ParamsRejection::Unreadable`] when
/// it cannot be read.
pub(crate) async fn read_values(
    parts: &Parts,
    body: Body,
) -> Result<Vec<(String, String)>, ParamsRejection> {
    let mut values = match parts.uri.query() {
        Some(query) => parse_encoded(query),
        None => Vec::new(),
    };

    if parts.method == Method::POST && is_form_post(parts.headers.get(header::CONTENT_TYPE)) {
        if let Some(length) = declared_length(parts)
            && length > MAX_FORM_BODY_BYTES
        {
            return Err(ParamsRejection::BodyTooLarge);
        }
        let bytes = to_bytes(body, MAX_FORM_BODY_BYTES)
            .await
            .map_err(|_| ParamsRejection::Unreadable)?;
        values.extend(parse_encoded(&String::from_utf8_lossy(&bytes)));
    }

    Ok(values)
}

/// Reads the parameters and the JSON body of a request.
///
/// The parameters come from the query string alone: the body of a request that
/// carries a JSON document is the client description the transcoding extension
/// defines, not a set of parameters, so reading it as parameters would invent
/// them.
///
/// # Parameters
///
/// - `parts`: the request's head.
/// - `body`: the request's body, which is read only when the content type is
///   JSON.
///
/// # Returns
///
/// The parameter values and the parsed body, which is `None` when the request
/// carried no JSON document.
///
/// # Errors
///
/// Returns [`ParamsRejection::BodyTooLarge`] when the body declares more bytes
/// than [`MAX_FORM_BODY_BYTES`], [`ParamsRejection::Unreadable`] when it cannot
/// be read, and [`ParamsRejection::InvalidJson`] when it is not a JSON
/// document.
pub(crate) async fn read_json_body(
    parts: &Parts,
    body: Body,
) -> Result<(Vec<(String, String)>, Option<serde_json::Value>), ParamsRejection> {
    let values = match parts.uri.query() {
        Some(query) => parse_encoded(query),
        None => Vec::new(),
    };
    if !is_json(parts.headers.get(header::CONTENT_TYPE)) {
        return Ok((values, None));
    }
    if let Some(length) = declared_length(parts)
        && length > MAX_FORM_BODY_BYTES
    {
        return Err(ParamsRejection::BodyTooLarge);
    }
    let bytes = to_bytes(body, MAX_FORM_BODY_BYTES)
        .await
        .map_err(|_| ParamsRejection::Unreadable)?;
    let document = serde_json::from_slice(&bytes).map_err(|_| ParamsRejection::InvalidJson)?;
    Ok((values, Some(document)))
}

/// Reports whether a request's content type is a JSON document.
///
/// # Parameters
///
/// - `content_type`: the request's content type header, when it has one.
///
/// # Returns
///
/// `true` when the type is `application/json`, with any parameters after it.
fn is_json(content_type: Option<&axum::http::HeaderValue>) -> bool {
    match content_type {
        None => false,
        Some(value) => value
            .to_str()
            .map(|text| {
                text.trim_start()
                    .to_ascii_lowercase()
                    .starts_with("application/json")
            })
            .unwrap_or(false),
    }
}

/// Reports whether a request's content type is a form post.
///
/// A request without a content type counts as one: the extension's body is
/// unambiguous, so a client that omits the header is served rather than
/// rejected.
fn is_form_post(content_type: Option<&axum::http::HeaderValue>) -> bool {
    match content_type {
        None => true,
        Some(value) => value
            .to_str()
            .map(|text| {
                text.trim_start()
                    .to_ascii_lowercase()
                    .starts_with("application/x-www-form-urlencoded")
            })
            .unwrap_or(false),
    }
}

/// Returns the declared body length of a request.
fn declared_length(parts: &Parts) -> Option<usize> {
    parts
        .headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// Parses an `application/x-www-form-urlencoded` string.
///
/// Pairs are separated by `&` and split at the first `=`; a pair without one is
/// a name with an empty value. Both the name and the value are percent-decoded,
/// with `+` meaning a space.
fn parse_encoded(text: &str) -> Vec<(String, String)> {
    text.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (decode(name), decode(value)),
            None => (decode(pair), String::new()),
        })
        .collect()
}

/// Percent-decodes one form encoded name or value.
///
/// `+` becomes a space and `%XX` becomes the byte it names. An escape that is
/// not two hexadecimal digits is kept as written and the rest of the text is
/// decoded, so a client that under-encodes a password still authenticates
/// rather than failing with a malformed request.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Returns the value of one hexadecimal digit.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body as AxumBody;
    use axum::http::Request as HttpRequest;

    /// Builds parameters from a query string.
    fn params(query: &str) -> Params {
        Params::from_values(parse_encoded(query))
    }

    #[test]
    fn a_form_encoded_string_decodes_into_named_values() {
        assert_eq!(
            parse_encoded("u=echo&p=sesame&c=Feishin&v=1.16.1"),
            [
                ("u".to_owned(), "echo".to_owned()),
                ("p".to_owned(), "sesame".to_owned()),
                ("c".to_owned(), "Feishin".to_owned()),
                ("v".to_owned(), "1.16.1".to_owned()),
            ]
        );
    }

    #[test]
    fn plus_and_percent_escapes_decode() {
        assert_eq!(
            parse_encoded("p=a+b%20c%2Bd&name=%41%42&raw=%zz"),
            [
                ("p".to_owned(), "a b c+d".to_owned()),
                ("name".to_owned(), "AB".to_owned()),
                ("raw".to_owned(), "%zz".to_owned()),
            ]
        );
    }

    #[test]
    fn an_unterminated_escape_and_an_empty_pair_are_tolerated() {
        assert_eq!(
            parse_encoded("p=100%&&flag&="),
            [
                ("p".to_owned(), "100%".to_owned()),
                ("flag".to_owned(), String::new()),
                (String::new(), String::new()),
            ]
        );
    }

    #[test]
    fn a_parameter_can_carry_several_values_in_order() {
        let params = params("id=b&id=a&count=2");
        assert_eq!(params.get("id"), Some("b"));
        assert_eq!(params.get_all("id").collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(params.get("missing"), None);
        assert_eq!(params.len(), 3);
        assert!(params.has("count"));
        assert!(!params.has("counts"));
        assert!(params.matches("count", "2"));
        assert!(!params.matches("count", "3"));
    }

    #[test]
    fn the_format_and_the_vendor_flag_come_from_the_parameters() {
        assert_eq!(params("").format(), ResponseFormat::Xml);
        assert_eq!(params("f=json").format(), ResponseFormat::Json);
        assert_eq!(params("f=jsonp").format(), ResponseFormat::Json);
        assert!(!params("f=json").vendor_enabled());
        assert!(params("hearthsound=1").vendor_enabled());
        assert!(!params("hearthsound=0").vendor_enabled());
        assert!(!params("hearthsound=").vendor_enabled());
    }

    #[test]
    fn the_client_and_version_parameters_are_readable() {
        let params = params("c=Symfonium&v=1.16.1");
        assert_eq!(params.client_name(), Some("Symfonium"));
        assert_eq!(params.protocol_version(), Some("1.16.1"));
        assert_eq!(super::tests::params("").client_name(), None);
    }

    #[test]
    fn the_credentials_are_the_five_authentication_parameters() {
        let credentials = params("u=echo&p=sesame&t=abc&s=salt&apiKey=hs_x").credentials();
        assert_eq!(credentials.username.as_deref(), Some("echo"));
        assert_eq!(credentials.password.as_deref(), Some("sesame"));
        assert_eq!(credentials.token.as_deref(), Some("abc"));
        assert_eq!(credentials.salt.as_deref(), Some("salt"));
        assert_eq!(credentials.api_key.as_deref(), Some("hs_x"));
        assert_eq!(params("").credentials(), SubsonicCredentials::default());
    }

    #[tokio::test]
    async fn a_form_post_body_supplies_parameters_after_the_query_string() {
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/rest/ping?c=Feishin")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(AxumBody::from("c=Body&p=sesame"))
            .unwrap();
        let (parts, body) = request.into_parts();
        let values = read_values(&parts, body).await.unwrap();
        let params = Params::from_values(values);
        assert_eq!(params.client_name(), Some("Feishin"));
        assert_eq!(params.get("p"), Some("sesame"));
    }

    #[tokio::test]
    async fn a_get_request_body_is_ignored() {
        let request = HttpRequest::builder()
            .method(Method::GET)
            .uri("/rest/ping?c=Feishin")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(AxumBody::from("c=Body"))
            .unwrap();
        let (parts, body) = request.into_parts();
        let values = read_values(&parts, body).await.unwrap();
        assert_eq!(values, [("c".to_owned(), "Feishin".to_owned())]);
    }

    #[tokio::test]
    async fn a_form_body_over_the_cap_is_rejected() {
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/rest/ping")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(
                header::CONTENT_LENGTH,
                (MAX_FORM_BODY_BYTES + 1).to_string(),
            )
            .body(AxumBody::from("c=Feishin"))
            .unwrap();
        let (parts, body) = request.into_parts();
        let rejection = read_values(&parts, body).await.unwrap_err();
        assert!(matches!(rejection, ParamsRejection::BodyTooLarge));
    }

    #[tokio::test]
    async fn a_form_body_that_lies_about_its_length_is_rejected() {
        let oversized = "a".repeat(MAX_FORM_BODY_BYTES + 1);
        let request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/rest/ping")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(AxumBody::from(format!("c={oversized}")))
            .unwrap();
        let (parts, body) = request.into_parts();
        let rejection = read_values(&parts, body).await.unwrap_err();
        assert!(matches!(rejection, ParamsRejection::Unreadable));
    }
}
