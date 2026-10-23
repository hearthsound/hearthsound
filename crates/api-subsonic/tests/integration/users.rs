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
    assert!(user.body.contains("\"adminRole\":true"), "{}", user.body);
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
    assert!(user.body.contains("\"adminRole\":false"), "{}", user.body);
    assert!(
        user.body.contains("\"folder\":[1]"),
        "a non-administrator browses the personal library only: {}",
        user.body
    );
}

/// The server-wide view is a folder for administrators only.
#[tokio::test]
async fn the_server_folder_is_listed_for_administrators_only() {
    let fixture = fixture().await;
    let app = fixture.app();
    let limited = account(&fixture, "limited", false).await;

    let admin = get(
        &app,
        &format!(
            "/rest/getUser.view?username=echo&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&admin);
    assert!(admin.body.contains("<folder>1</folder>"), "{}", admin.body);
    assert!(
        admin.body.contains("<folder>2</folder>"),
        "an administrator browses the server view: {}",
        admin.body
    );

    let user = get(
        &app,
        &format!("/rest/getUser.view?username=limited&apiKey={limited}"),
    )
    .await;
    assert_ok(&user);
    assert!(user.body.contains("<folder>1</folder>"), "{}", user.body);
    assert!(
        !user.body.contains("<folder>2</folder>"),
        "the server view is not offered to a non-administrator: {}",
        user.body
    );

    // The listing reports each account's own folders.
    let listed = get(
        &app,
        &format!("/rest/getUsers.view?apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    let non_admin = listed
        .body
        .split("<user ")
        .find(|entry| entry.contains("username=\"limited\""))
        .expect("the non-administrator is listed");
    assert!(
        non_admin.contains("<folder>1</folder>") && !non_admin.contains("<folder>2</folder>"),
        "the listing gives a non-administrator the personal library only: {non_admin}"
    );
    let administrator = listed
        .body
        .split("<user ")
        .find(|entry| entry.contains("username=\"echo\""))
        .expect("the administrator is listed");
    assert!(
        administrator.contains("<folder>2</folder>"),
        "the listing gives an administrator the server view: {administrator}"
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

/// An administrator lists every account, in both encodings.
#[tokio::test]
async fn an_administrator_lists_the_accounts() {
    let fixture = fixture().await;
    let admin = account(&fixture, "root", true).await;
    let app = fixture.app();

    let listed = get(&app, &format!("/rest/getUsers?apiKey={admin}&f=json")).await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"users\":{\"user\":["),
        "{}",
        listed.body
    );
    assert!(
        listed.body.contains("\"username\":\"echo\""),
        "{}",
        listed.body
    );
    assert!(
        listed.body.contains("\"username\":\"root\""),
        "{}",
        listed.body
    );
    assert!(
        listed.body.contains("\"adminRole\":true"),
        "the administrator is reported as one: {}",
        listed.body
    );
    assert!(
        listed.body.contains("\"folder\":[1,2]"),
        "the folders the account may browse are reported: {}",
        listed.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("users-json", listed.body.clone());
    });

    let xml = get(&app, &format!("/rest/getUsers?apiKey={admin}")).await;
    assert_ok(&xml);
    assert!(xml.body.contains("<users>"), "{}", xml.body);
    assert!(xml.body.contains("<user username=\"echo\""), "{}", xml.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("users-xml", xml.body.clone());
    });
}

/// The account listing is for administrators, and an invalid key is refused
/// before it is read.
#[tokio::test]
async fn listing_the_accounts_is_for_administrators() {
    let fixture = fixture().await;
    let limited = account(&fixture, "limited", false).await;
    let app = fixture.app();

    let refused = get(&app, &format!("/rest/getUsers?apiKey={limited}&f=json")).await;
    assert_error(&refused, 50);

    let anonymous = get(&app, "/rest/getUsers?apiKey=hs_bogus&f=json").await;
    assert_error(&anonymous, 44);
    assert!(
        !anonymous.body.contains("\"users\""),
        "a refused call carries no accounts: {}",
        anonymous.body
    );
}

/// The four account writes point a client at how accounts are managed.
#[tokio::test]
async fn account_writes_point_at_the_documentation() {
    let fixture = fixture().await;
    let admin = account(&fixture, "root", true).await;
    let app = fixture.app();

    for endpoint in ["createUser", "updateUser", "deleteUser", "changePassword"] {
        let refused = get(
            &app,
            &format!("/rest/{endpoint}?username=echo&apiKey={admin}&f=json"),
        )
        .await;
        assert_error(&refused, 50);
        assert!(
            refused
                .body
                .contains("\"helpUrl\":\"https://hearthsound.app/docs/admin/users\""),
            "{endpoint} points at the documentation: {}",
            refused.body
        );

        let xml = get(&app, &format!("/rest/{endpoint}.view?apiKey={admin}")).await;
        assert_error(&xml, 50);
        assert!(
            xml.body
                .contains("helpUrl=\"https://hearthsound.app/docs/admin/users\""),
            "{endpoint} points at the documentation: {}",
            xml.body
        );
    }
}
