//! Snapshot tests for the envelope and its error member.
//!
//! The two encodings are rendered from the same values, so each pair of
//! snapshots pins both the field coverage and the byte-stable field order.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_api_subsonic::{Array, Object, Response, ResponseFormat, SubsonicError};
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
