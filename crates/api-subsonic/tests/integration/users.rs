//! Integration tests for `getUser`: the account's roles, the folders it may
//! browse, and the accounts a caller may ask about.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_auth::{ApiKeys, NewUser, Users};
use hearthsound_config::{SettingValue, keys};

use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library.
///
/// # Returns
///
/// The fixture; it keeps its temporary directory alive.
async fn fixture() -> Fixture {
    Fixture::new().await
}

/// Creates an account with the given name and admin flag.
///
/// # Parameters
///
/// - `fixture`: the fixture whose database the account is written to.
/// - `username`: the login name.
/// - `is_admin`: whether the account administers the server.
///
/// # Returns
///
/// One of the account's API keys.
async fn account(fixture: &Fixture, username: &str, is_admin: bool) -> String {
    let user = Users::new(&fixture.database)
        .create(
            &fixture.settings.current(),
            &NewUser::new(username, username, "hunter2", is_admin),
        )
        .await
        .unwrap();
    ApiKeys::new(&fixture.database)
        .generate(user.id, username)
        .await
        .unwrap()
        .plaintext
}

/// An account reads its own roles and the folders it may browse.
#[tokio::test]
async fn an_account_reads_its_own_roles() {
    let fixture = fixture().await;
    let app = fixture.app();

    let user = get(
        &app,
        &format!(
            "/rest/getUser?username=echo&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&user);
    assert!(user.body.contains("\"username\":\"echo\""), "{}", user.body);
    assert!(user.body.contains("\"email\":\"\""), "{}", user.body);
    assert!(user.body.contains("\"adminRole\":false"), "{}", user.body);
    assert!(user.body.contains("\"settingsRole\":true"), "{}", user.body);
    assert!(user.body.contains("\"streamRole\":true"), "{}", user.body);
    assert!(user.body.contains("\"podcastRole\":false"), "{}", user.body);
    assert!(user.body.contains("\"folder\":[1,2]"), "{}", user.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("user-json", user.body.clone());
    });

    // The account's own permissions are what the roles report.
    fixture
        .settings
        .set(
            keys::USERS_DEFAULT_PERMISSIONS.name(),
            SettingValue::StringList(vec!["download".to_owned(), "upload".to_owned()]),
            None,
        )
        .await
        .unwrap();
    let limited = account(&fixture, "limited", false).await;
    let user = get(
        &app,
        &format!("/rest/getUser?username=limited&apiKey={limited}&f=json"),
    )
    .await;
    assert_ok(&user);
    assert!(user.body.contains("\"downloadRole\":true"), "{}", user.body);
    assert!(user.body.contains("\"uploadRole\":true"), "{}", user.body);
}

/// The server-wide view is a folder only while it is offered.
#[tokio::test]
async fn the_server_folder_follows_the_privacy_setting() {
    let fixture = fixture().await;
    let app = fixture.app();

    fixture
        .settings
        .set(
            keys::PRIVACY_SERVER_BROWSE.name(),
            SettingValue::String("disabled".to_owned()),
            None,
        )
        .await
        .unwrap();
    let user = get(
        &app,
        &format!(
            "/rest/getUser.view?username=echo&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&user);
    assert!(user.body.contains("<folder>1</folder>"), "{}", user.body);
    assert!(
        !user.body.contains("<folder>2</folder>"),
        "the server view is not offered: {}",
        user.body
    );
}

/// Only an administrator asks about another account.
#[tokio::test]
async fn another_account_is_for_administrators() {
    let fixture = fixture().await;
    let limited = account(&fixture, "limited", false).await;
    let admin = account(&fixture, "root", true).await;
    let app = fixture.app();

    let refused = get(
        &app,
        &format!("/rest/getUser?username=echo&apiKey={limited}&f=json"),
    )
    .await;
    assert_error(&refused, 50);

    let read = get(
        &app,
        &format!("/rest/getUser?username=echo&apiKey={admin}&f=json"),
    )
    .await;
    assert_ok(&read);
    assert!(read.body.contains("\"username\":\"echo\""), "{}", read.body);

    let missing = get(&app, &format!("/rest/getUser?apiKey={admin}&f=json")).await;
    assert_error(&missing, 10);
    let unknown = get(
        &app,
        &format!("/rest/getUser?username=nobody&apiKey={admin}&f=json"),
    )
    .await;
    assert_error(&unknown, 70);
}
