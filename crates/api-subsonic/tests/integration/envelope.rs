//! Snapshot tests for the envelope and its error member.
//!
//! The two encodings are rendered from the same values, so each pair of
//! snapshots pins both the field coverage and the byte-stable field order.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_api_subsonic::{Array, Object, Params, Response, ResponseFormat, SubsonicError};
use hearthsound_auth::{APP_PASSWORDS_HELP_URL, AuthenticationError};

#[test]
fn an_empty_ok_envelope_as_json() {
    insta::assert_snapshot!("ok-json", Response::ok().render(ResponseFormat::Json));
}

#[test]
fn an_empty_ok_envelope_as_xml() {
    insta::assert_snapshot!("ok-xml", Response::ok().render(ResponseFormat::Xml));
}

#[test]
fn a_payload_envelope_as_json() {
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
    insta::assert_snapshot!("payload-json", response.render(ResponseFormat::Json));
}

#[test]
fn a_payload_envelope_as_xml() {
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
    insta::assert_snapshot!("payload-xml", response.render(ResponseFormat::Xml));
}

#[test]
fn a_failed_envelope_as_json() {
    let response = Response::failed(SubsonicError::authentication(
        &AuthenticationError::InvalidApiKey,
    ));
    insta::assert_snapshot!("failed-json", response.render(ResponseFormat::Json));
}

#[test]
fn a_failed_envelope_as_xml() {
    let response = Response::failed(SubsonicError::authentication(
        &AuthenticationError::InvalidApiKey,
    ));
    insta::assert_snapshot!("failed-xml", response.render(ResponseFormat::Xml));
}

#[test]
fn a_failed_envelope_with_a_help_url_as_json() {
    let response = Response::failed(SubsonicError::authentication(
        &AuthenticationError::AccountPasswordDisabled {
            help_url: APP_PASSWORDS_HELP_URL.to_owned(),
        },
    ));
    insta::assert_snapshot!("help-url-json", response.render(ResponseFormat::Json));
}

#[test]
fn a_failed_envelope_with_a_help_url_as_xml() {
    let response = Response::failed(SubsonicError::authentication(
        &AuthenticationError::AccountPasswordDisabled {
            help_url: APP_PASSWORDS_HELP_URL.to_owned(),
        },
    ));
    insta::assert_snapshot!("help-url-xml", response.render(ResponseFormat::Xml));
}

/// Renders a response the way a call with these parameters renders it.
///
/// # Parameters
///
/// - `parameters`: the request's parameters, query string order.
/// - `response`: the response the handler built.
///
/// # Returns
///
/// The document the client receives.
fn encoded(parameters: &[(&str, &str)], response: Response) -> String {
    let values = parameters
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    Params::from_values(values)
        .encode(response)
        .body()
        .to_owned()
}

/// The refusal a denied acquisition answers, as `hsAcquire` builds it.
fn denied() -> Response {
    Response::failed(SubsonicError::not_authorized().with_reason("quota-exceeded"))
}

#[test]
fn a_denied_acquisition_as_json_carries_its_reason() {
    insta::assert_snapshot!(
        "vendor-reason-json",
        encoded(&[("f", "json"), ("hearthsound", "1")], denied())
    );
}

#[test]
fn a_denied_acquisition_as_xml_carries_its_reason() {
    insta::assert_snapshot!(
        "vendor-reason-xml",
        encoded(&[("f", "xml"), ("hearthsound", "1")], denied())
    );
}

#[test]
fn a_denied_acquisition_without_the_vendor_flag_is_the_standard_refusal() {
    let standard = Response::failed(SubsonicError::not_authorized());
    assert_eq!(
        encoded(&[("f", "json")], denied()),
        standard.render(ResponseFormat::Json),
        "without hearthsound=1 the JSON answer is the standard refusal byte for byte"
    );
    assert_eq!(
        encoded(&[("f", "xml")], denied()),
        standard.render(ResponseFormat::Xml),
        "without hearthsound=1 the XML answer is the standard refusal byte for byte"
    );
    assert_eq!(
        encoded(&[], denied()),
        standard.render(ResponseFormat::Xml),
        "the default encoding is unchanged too"
    );
}
