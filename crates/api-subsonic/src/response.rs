//! The response envelope, its error member, and the two encodings of it.
//!
//! Every non-binary `/rest` call answers with a [`Response`]. The envelope
//! fields are fixed and shared by both encodings; the payload members (or the
//! error) follow them in the order the call built them.

use axum::http::header;
use axum::response::{IntoResponse, Response as HttpResponse};
use hearthsound_auth::AuthenticationError;

use crate::render;
use crate::value::{Object, Value};

/// The base Subsonic protocol version this server claims.
pub const API_VERSION: &str = "1.16.1";

/// The server type OpenSubsonic clients see in the envelope.
pub const SERVER_TYPE: &str = "hearthsound";

/// The server version in the envelope, which is this crate's version.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The XML namespace of the base protocol's schema.
const XML_NAMESPACE: &str = "http://subsonic.org/restapi";

/// The element and object name of the envelope.
const ENVELOPE: &str = "subsonic-response";

/// The Subsonic error codes, as the base protocol and the OpenSubsonic
/// extensions define them.
pub mod codes {
    /// A generic error.
    pub const GENERIC: i32 = 0;
    /// A required parameter is missing.
    pub const MISSING_PARAMETER: i32 = 10;
    /// Incompatible protocol version: the client must upgrade.
    pub const CLIENT_MUST_UPGRADE: i32 = 20;
    /// Incompatible protocol version: the server must upgrade.
    pub const SERVER_MUST_UPGRADE: i32 = 30;
    /// Wrong username or password.
    pub const WRONG_CREDENTIALS: i32 = 40;
    /// Token authentication is not supported.
    pub const TOKEN_AUTH_UNSUPPORTED: i32 = 41;
    /// The authentication mechanism is not supported.
    pub const MECHANISM_UNSUPPORTED: i32 = 42;
    /// Multiple conflicting authentication mechanisms were supplied.
    pub const CONFLICTING_MECHANISMS: i32 = 43;
    /// The API key is invalid.
    pub const INVALID_API_KEY: i32 = 44;
    /// The user is not authorized for the given operation.
    pub const NOT_AUTHORIZED: i32 = 50;
    /// The trial period is over.
    pub const TRIAL_EXPIRED: i32 = 60;
    /// The requested data was not found.
    pub const NOT_FOUND: i32 = 70;
}

/// Whether a call succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The call succeeded.
    Ok,
    /// The call failed and the envelope carries an error.
    Failed,
}

impl Status {
    /// Returns the `status` field's value.
    ///
    /// # Returns
    ///
    /// `ok` or `failed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
        }
    }
}

/// The encoding a client asked a response to be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFormat {
    /// `f=json`, and `f=jsonp`, which is answered as plain JSON.
    Json,
    /// `f=xml`, and the default for an absent or unrecognized `f`.
    Xml,
}

impl ResponseFormat {
    /// Selects the format a client asked for with the `f` parameter.
    ///
    /// The base protocol's default is XML, so an absent or unrecognized value
    /// selects XML. `jsonp` selects JSON: the callback wrapper is not
    /// implemented, and JSON is the closest answer this server can give.
    ///
    /// # Parameters
    ///
    /// - `format`: the `f` parameter, when the request carried one.
    ///
    /// # Returns
    ///
    /// The format to render.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_api_subsonic::ResponseFormat;
    ///
    /// assert_eq!(ResponseFormat::from_parameter(None), ResponseFormat::Xml);
    /// assert_eq!(ResponseFormat::from_parameter(Some("json")), ResponseFormat::Json);
    /// assert_eq!(ResponseFormat::from_parameter(Some("jsonp")), ResponseFormat::Json);
    /// assert_eq!(ResponseFormat::from_parameter(Some("xml")), ResponseFormat::Xml);
    /// ```
    pub fn from_parameter(format: Option<&str>) -> ResponseFormat {
        match format {
            Some(value)
                if value.eq_ignore_ascii_case("json") || value.eq_ignore_ascii_case("jsonp") =>
            {
                ResponseFormat::Json
            }
            _ => ResponseFormat::Xml,
        }
    }

    /// Returns the `Content-Type` a response in this format is sent with.
    pub fn content_type(self) -> &'static str {
        match self {
            ResponseFormat::Json => "application/json; charset=utf-8",
            ResponseFormat::Xml => "text/xml; charset=utf-8",
        }
    }
}

/// The `error` member of a failed envelope.
///
/// The code is the base protocol's; the message is the standard text for that
/// code wherever the protocol defines one. `helpUrl`, an OpenSubsonic addition,
/// is present only for the errors that have advice to give. `hearthsoundReason`,
/// a Hearthsound-only member, is present only for a client that sent
/// `hearthsound=1`: a refusal that has a stored reason - a denied acquisition,
/// today - carries it there, and the base protocol's clients ignore it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsonicError {
    /// The numeric code, one of [`codes`].
    code: i32,
    /// The human readable message.
    message: String,
    /// The documentation link, when the error has one.
    help_url: Option<String>,
    /// The vendor reason a denial carries, sent as `hearthsoundReason`.
    reason: Option<&'static str>,
}

impl SubsonicError {
    /// Builds an error from a code and a message.
    ///
    /// Prefer the named constructors below, which carry the standard message
    /// for their code.
    ///
    /// # Parameters
    ///
    /// - `code`: the numeric code.
    /// - `message`: the message the client displays.
    ///
    /// # Returns
    ///
    /// The error, without a help URL.
    pub fn new(code: i32, message: impl Into<String>) -> SubsonicError {
        SubsonicError {
            code,
            message: message.into(),
            help_url: None,
            reason: None,
        }
    }

    /// Adds the `helpUrl` member.
    ///
    /// # Parameters
    ///
    /// - `help_url`: the documentation link.
    ///
    /// # Returns
    ///
    /// The error, with the link.
    pub fn with_help_url(mut self, help_url: impl Into<String>) -> SubsonicError {
        self.help_url = Some(help_url.into());
        self
    }

    /// Adds the Hearthsound-only reason member.
    ///
    /// # Parameters
    ///
    /// - `reason`: the stored token, for example `quota-exceeded`.
    ///
    /// # Returns
    ///
    /// The error, with the reason.
    pub fn with_reason(mut self, reason: &'static str) -> SubsonicError {
        self.reason = Some(reason);
        self
    }

    /// Returns the numeric code.
    pub fn code(&self) -> i32 {
        self.code
    }

    /// Returns the message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the documentation link, when the error has one.
    pub fn help_url(&self) -> Option<&str> {
        self.help_url.as_deref()
    }

    /// Returns the vendor reason, when the error has one.
    pub fn reason(&self) -> Option<&'static str> {
        self.reason
    }

    /// Builds the error for a check that could not be completed.
    ///
    /// # Returns
    ///
    /// Error [`codes::GENERIC`] with the standard message for it. The reason is
    /// for the server's log, not for the client.
    pub fn generic() -> SubsonicError {
        SubsonicError::new(codes::GENERIC, "A generic error.")
    }

    /// Builds the error for a missing required parameter.
    ///
    /// # Parameters
    ///
    /// - `parameter`: the parameter's name, which is logged but not sent,
    ///   because the standard message does not name it.
    ///
    /// # Returns
    ///
    /// Error [`codes::MISSING_PARAMETER`].
    pub fn missing_parameter(parameter: &'static str) -> SubsonicError {
        tracing::debug!(parameter, "a /rest call is missing a parameter");
        SubsonicError::new(codes::MISSING_PARAMETER, "Required parameter is missing.")
    }

    /// Builds the error for an invalid API key.
    ///
    /// # Returns
    ///
    /// Error [`codes::INVALID_API_KEY`].
    pub fn invalid_api_key() -> SubsonicError {
        SubsonicError::new(codes::INVALID_API_KEY, "Invalid API key.")
    }

    /// Builds the error for an operation the principal may not perform.
    ///
    /// # Returns
    ///
    /// Error [`codes::NOT_AUTHORIZED`].
    pub fn not_authorized() -> SubsonicError {
        SubsonicError::new(
            codes::NOT_AUTHORIZED,
            "User is not authorized for the given operation.",
        )
    }

    /// Builds the error for an unknown endpoint or an entity that does not
    /// exist.
    ///
    /// # Returns
    ///
    /// Error [`codes::NOT_FOUND`].
    pub fn not_found() -> SubsonicError {
        SubsonicError::new(codes::NOT_FOUND, "The requested data was not found.")
    }

    /// Builds the error a credential check returned.
    ///
    /// # Parameters
    ///
    /// - `error`: the rejection from [`hearthsound_auth::Authenticator`].
    ///
    /// # Returns
    ///
    /// The error with that rejection's code, message, and help URL.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_api_subsonic::SubsonicError;
    /// use hearthsound_auth::AuthenticationError;
    ///
    /// let error = SubsonicError::authentication(&AuthenticationError::InvalidApiKey);
    /// assert_eq!(error.code(), 44);
    /// assert_eq!(error.message(), "Invalid API key.");
    /// ```
    pub fn authentication(error: &AuthenticationError) -> SubsonicError {
        let mut rejection = SubsonicError::new(error.code(), error.message());
        if let Some(help_url) = error.help_url() {
            rejection = rejection.with_help_url(help_url);
        }
        rejection
    }
}

impl From<&SubsonicError> for Value {
    /// Renders an error as the envelope's `error` member.
    fn from(error: &SubsonicError) -> Value {
        let mut object = Object::new()
            .set("code", i64::from(error.code()))
            .set("message", error.message().to_owned());
        if let Some(help_url) = error.help_url() {
            object = object.set("helpUrl", help_url.to_owned());
        }
        if let Some(reason) = error.reason() {
            object = object.set("hearthsoundReason", reason.to_owned());
        }
        Value::Object(object)
    }
}

/// One `/rest` answer: a successful payload, or a failure.
///
/// # Examples
///
/// ```
/// use hearthsound_api_subsonic::{Object, Response, ResponseFormat};
///
/// let license = Response::ok().member("license", Object::new().set("valid", true));
/// let xml = license.render(ResponseFormat::Xml);
/// assert!(xml.starts_with(
///     "<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\" version=\"1.16.1\""
/// ));
/// assert!(xml.ends_with("<license valid=\"true\"/></subsonic-response>"));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// The call succeeded and carries these payload members.
    Payload(Object),
    /// The call failed and carries this error.
    Error(SubsonicError),
}

impl Response {
    /// Builds an empty successful response, as `ping` answers.
    ///
    /// # Returns
    ///
    /// The response, with no payload members.
    pub fn ok() -> Response {
        Response::Payload(Object::new())
    }

    /// Builds a failed response.
    ///
    /// # Parameters
    ///
    /// - `error`: the error the envelope carries.
    ///
    /// # Returns
    ///
    /// The response, with status `failed`.
    pub fn failed(error: SubsonicError) -> Response {
        Response::Error(error)
    }

    /// Adds a payload member and returns the response.
    ///
    /// A failed response has no payload, so this returns it unchanged.
    ///
    /// # Parameters
    ///
    /// - `name`: the member name.
    /// - `value`: the member value; anything that converts into a [`Value`].
    ///
    /// # Returns
    ///
    /// The response, with the member added last.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_api_subsonic::{Array, Object, Response, ResponseFormat, Value};
    ///
    /// let folders = Response::ok().member(
    ///     "musicFolders",
    ///     Object::new().set("musicFolder", Array::new([Object::new().set("id", 1)])),
    /// );
    /// assert!(folders.render(ResponseFormat::Json).contains("\"musicFolder\":[{\"id\":1}]"));
    /// ```
    pub fn member<N, V>(self, name: N, value: V) -> Response
    where
        N: Into<std::borrow::Cow<'static, str>>,
        V: Into<Value>,
    {
        match self {
            Response::Payload(object) => Response::Payload(object.set(name, value)),
            failure @ Response::Error(_) => failure,
        }
    }

    /// Returns the status the envelope reports.
    pub fn status(&self) -> Status {
        match self {
            Response::Payload(_) => Status::Ok,
            Response::Error(_) => Status::Failed,
        }
    }

    /// Returns the payload members, when the call succeeded.
    pub fn payload(&self) -> Option<&Object> {
        match self {
            Response::Payload(object) => Some(object),
            Response::Error(_) => None,
        }
    }

    /// Returns the error, when the call failed.
    pub fn error(&self) -> Option<&SubsonicError> {
        match self {
            Response::Payload(_) => None,
            Response::Error(error) => Some(error),
        }
    }

    /// Returns the response with every member only Hearthsound knows removed.
    ///
    /// [`crate::Params::encode`] calls this for a request that did not send
    /// `hearthsound=1`, so a standard response is byte-identical to one from a
    /// server that has no vendor extension at all.
    ///
    /// # Returns
    ///
    /// The same response, carrying no vendor member.
    pub fn without_vendor_members(self) -> Response {
        match self {
            Response::Error(error) => Response::Error(SubsonicError {
                reason: None,
                ..error
            }),
            payload => payload,
        }
    }

    /// Renders the envelope in the requested format.
    ///
    /// # Parameters
    ///
    /// - `format`: the encoding to render.
    ///
    /// # Returns
    ///
    /// The complete document, without a trailing newline.
    pub fn render(&self, format: ResponseFormat) -> String {
        match format {
            ResponseFormat::Json => self.to_json(),
            ResponseFormat::Xml => self.to_xml(),
        }
    }

    /// Renders the envelope as JSON.
    ///
    /// # Returns
    ///
    /// `{"subsonic-response": {...}}`, in envelope field order.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"");
        out.push_str(ENVELOPE);
        out.push_str("\":{");
        for (index, (name, value)) in self.envelope_fields().into_iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            render::write_json_string(&mut out, name);
            out.push(':');
            render::write_json(&mut out, &value);
        }
        match self {
            Response::Payload(object) => {
                for (name, value) in object.members() {
                    out.push(',');
                    render::write_json_string(&mut out, name);
                    out.push(':');
                    render::write_json(&mut out, value);
                }
            }
            Response::Error(error) => {
                out.push(',');
                render::write_json_string(&mut out, "error");
                out.push(':');
                render::write_json(&mut out, &Value::from(error));
            }
        }
        out.push_str("}}");
        out
    }

    /// Renders the envelope as XML.
    ///
    /// # Returns
    ///
    /// A `<subsonic-response>` document in the base protocol's namespace, with
    /// the envelope's fields as attributes.
    pub fn to_xml(&self) -> String {
        let mut out = String::new();
        out.push('<');
        out.push_str(ENVELOPE);
        out.push_str(" xmlns=\"");
        out.push_str(XML_NAMESPACE);
        out.push('"');
        for (name, value) in self.envelope_fields() {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            render::write_attribute_value(&mut out, &value);
            out.push('"');
        }
        match self {
            Response::Payload(object) if object.is_empty() => out.push_str("/>"),
            Response::Payload(object) => {
                out.push('>');
                for (name, value) in object.members() {
                    render::write_member(&mut out, name, value);
                }
                out.push_str("</");
                out.push_str(ENVELOPE);
                out.push('>');
            }
            Response::Error(error) => {
                out.push('>');
                render::write_member(&mut out, "error", &Value::from(error));
                out.push_str("</");
                out.push_str(ENVELOPE);
                out.push('>');
            }
        }
        out
    }

    /// Renders the envelope and pairs it with the content type of its format.
    ///
    /// # Parameters
    ///
    /// - `format`: the encoding to render.
    ///
    /// # Returns
    ///
    /// The encoded response, ready to be returned from a handler.
    pub fn encode(self, format: ResponseFormat) -> Encoded {
        Encoded {
            format,
            body: self.render(format),
        }
    }

    /// Returns the envelope's own fields, in field order.
    ///
    /// The two encodings render these from this one list, so a client sees the
    /// same envelope fields either way.
    fn envelope_fields(&self) -> [(&'static str, Value); 5] {
        [
            ("status", Value::from(self.status().as_str())),
            ("version", Value::from(API_VERSION)),
            ("type", Value::from(SERVER_TYPE)),
            ("serverVersion", Value::from(SERVER_VERSION)),
            ("openSubsonic", Value::from(true)),
        ]
    }
}

/// A response body that is already in the encoding the client asked for.
///
/// A handler returns this instead of writing HTTP itself: it carries the body
/// and the content type of the format the request selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// The encoding the body is in.
    format: ResponseFormat,
    /// The rendered document.
    body: String,
}

impl Encoded {
    /// Returns the encoding of the body.
    pub fn format(&self) -> ResponseFormat {
        self.format
    }

    /// Returns the rendered document.
    pub fn body(&self) -> &str {
        &self.body
    }
}

impl IntoResponse for Encoded {
    /// Answers with the rendered body, its content type, and HTTP 200.
    fn into_response(self) -> HttpResponse {
        (
            [(header::CONTENT_TYPE, self.format.content_type())],
            self.body,
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Array;

    /// Renders an envelope and returns both encodings.
    fn both(response: &Response) -> (String, String) {
        (
            response.render(ResponseFormat::Json),
            response.render(ResponseFormat::Xml),
        )
    }

    #[test]
    fn an_empty_ok_envelope_carries_every_envelope_field_in_order() {
        let (json, xml) = both(&Response::ok());
        assert_eq!(
            json,
            format!(
                "{{\"subsonic-response\":{{\"status\":\"ok\",\"version\":\"{}\",\"type\":\"{}\",\
                 \"serverVersion\":\"{}\",\"openSubsonic\":true}}}}",
                API_VERSION, SERVER_TYPE, SERVER_VERSION
            )
        );
        assert_eq!(
            xml,
            format!(
                "<subsonic-response xmlns=\"{}\" status=\"ok\" version=\"{}\" type=\"{}\" \
                 serverVersion=\"{}\" openSubsonic=\"true\"/>",
                XML_NAMESPACE, API_VERSION, SERVER_TYPE, SERVER_VERSION
            )
        );
    }

    #[test]
    fn a_failed_envelope_carries_only_the_error() {
        let response = Response::failed(SubsonicError::not_found())
            .member("musicFolders", Object::new().set("id", 1));
        let (json, xml) = both(&response);
        assert_eq!(
            json,
            format!(
                "{{\"subsonic-response\":{{\"status\":\"failed\",\"version\":\"{}\",\"type\":\"{}\",\
                 \"serverVersion\":\"{}\",\"openSubsonic\":true,\"error\":{{\"code\":70,\
                 \"message\":\"The requested data was not found.\"}}}}}}",
                API_VERSION, SERVER_TYPE, SERVER_VERSION
            )
        );
        assert!(xml.ends_with(
            "openSubsonic=\"true\"><error code=\"70\" \
             message=\"The requested data was not found.\"/></subsonic-response>"
        ));
    }

    #[test]
    fn an_error_carries_its_help_url_only_when_it_has_one() {
        let with_help = SubsonicError::new(codes::WRONG_CREDENTIALS, "Wrong username or password.")
            .with_help_url("https://hearthsound.app/docs/clients/app-passwords");
        let json = Response::failed(with_help).to_json();
        assert!(
            json.contains("\"helpUrl\":\"https://hearthsound.app/docs/clients/app-passwords\"")
        );
        assert!(
            !Response::failed(SubsonicError::generic())
                .to_json()
                .contains("helpUrl")
        );
    }

    #[test]
    fn a_payload_carries_the_same_fields_in_both_encodings() {
        let response = Response::ok()
            .member("license", Object::new().set("valid", true))
            .member(
                "musicFolders",
                Object::new().set(
                    "musicFolder",
                    Array::new([
                        Object::new().set("id", 1).set("name", "My Library"),
                        Object::new().set("id", 2).set("name", "Server"),
                    ]),
                ),
            );
        let (json, xml) = both(&response);
        assert_eq!(
            json,
            format!(
                "{{\"subsonic-response\":{{\"status\":\"ok\",\"version\":\"{}\",\"type\":\"{}\",\
                 \"serverVersion\":\"{}\",\"openSubsonic\":true,\"license\":{{\"valid\":true}},\
                 \"musicFolders\":{{\"musicFolder\":[{{\"id\":1,\"name\":\"My Library\"}},\
                 {{\"id\":2,\"name\":\"Server\"}}]}}}}}}",
                API_VERSION, SERVER_TYPE, SERVER_VERSION
            )
        );
        assert!(xml.ends_with(
            "openSubsonic=\"true\"><license valid=\"true\"/><musicFolders>\
             <musicFolder id=\"1\" name=\"My Library\"/>\
             <musicFolder id=\"2\" name=\"Server\"/></musicFolders></subsonic-response>"
        ));
    }

    #[test]
    fn the_format_is_selected_from_the_f_parameter() {
        assert_eq!(ResponseFormat::from_parameter(None), ResponseFormat::Xml);
        assert_eq!(
            ResponseFormat::from_parameter(Some("xml")),
            ResponseFormat::Xml
        );
        assert_eq!(
            ResponseFormat::from_parameter(Some("json")),
            ResponseFormat::Json
        );
        assert_eq!(
            ResponseFormat::from_parameter(Some("JSON")),
            ResponseFormat::Json
        );
        assert_eq!(
            ResponseFormat::from_parameter(Some("jsonp")),
            ResponseFormat::Json
        );
        assert_eq!(
            ResponseFormat::from_parameter(Some("yaml")),
            ResponseFormat::Xml
        );
        assert_eq!(
            ResponseFormat::Json.content_type(),
            "application/json; charset=utf-8"
        );
        assert_eq!(
            ResponseFormat::Xml.content_type(),
            "text/xml; charset=utf-8"
        );
    }

    #[test]
    fn an_encoded_response_carries_its_content_type() {
        let encoded = Response::ok().encode(ResponseFormat::Json);
        assert_eq!(encoded.format(), ResponseFormat::Json);
        assert_eq!(encoded.body(), Response::ok().to_json());
        let http = encoded.into_response();
        assert_eq!(
            http.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json; charset=utf-8"
        );
    }
}
