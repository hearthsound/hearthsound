//! Integration tests for the per-user provider configuration table.
//!
//! A link is stored as one sealed envelope, so these tests check both what the
//! service returns and what the database holds: the plaintext configuration is
//! never in the row, and the context a plugin receives carries the listener's
//! lowercase hyphenated id and the configuration as JSON.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_config::secret::SecretKey;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{
    UserConfigError, UserConfigService, merge_user_config, redact_user_config, validate_user_config,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

/// Inserts an instance row, with the plugin row its foreign key needs.
async fn seed_instance(database: &Database, instance: Uuid) {
    sqlx::query(
        "INSERT OR IGNORE INTO plugins (id, version, api, manifest, sha256, source, enabled, installed_at)
         VALUES ('com.example.user-config-test', '0.1.0', 'hearthsound:plugin@0.3.0', '{}', x'00', 'sideload', 1, 0)",
    )
    .execute(database.writer())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO provider_instances
            (id, plugin_id, slug, display_name, enabled, created_at, updated_at)
         VALUES (?, 'com.example.user-config-test', ?, 'Instance', 1, 0, 0)",
    )
    .bind(instance.as_bytes().as_slice())
    .bind(format!("instance-{instance}"))
    .execute(database.writer())
    .await
    .unwrap();
}

/// Inserts a user row, which a link's foreign key needs.
async fn seed_user(database: &Database, user: Uuid) {
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at)
         VALUES (?, ?, 'User', 'hash', 0, 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(user.to_string())
    .execute(database.writer())
    .await
    .unwrap();
}

/// Opens a migrated temporary database and the service over it.
async fn service(dir: &Path) -> (Database, UserConfigService) {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    let secrets = SecretKey::generate().secret_box();
    let service = UserConfigService::new(&database, secrets);
    (database, service)
}

#[tokio::test]
async fn a_link_round_trips_through_a_sealed_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let (database, service) = service(dir.path()).await;
    let instance = Uuid::now_v7();
    let user = Uuid::now_v7();
    seed_instance(&database, instance).await;
    seed_user(&database, user).await;
    let config = json!({"token": "good", "userName": "listener"});

    let stored = service
        .put(instance, user, Some("listener".to_owned()), &config)
        .await
        .unwrap();
    assert_eq!(stored.account_name.as_deref(), Some("listener"));
    assert_eq!(stored.config, config);

    let link = service.get(instance, user).await.unwrap().unwrap();
    assert_eq!(link, stored);

    let raw: String = sqlx::query("SELECT config FROM provider_user_configs")
        .fetch_one(database.writer())
        .await
        .unwrap()
        .get("config");
    assert!(raw.contains("$secret"), "{raw}");
    assert!(!raw.contains("good"), "the token is sealed: {raw}");
    assert!(
        !raw.contains("listener"),
        "the account name is sealed: {raw}"
    );

    let context = link.context(user);
    assert_eq!(context.user_id, user.to_string());
    assert_eq!(context.user_id, context.user_id.to_lowercase());
    assert_eq!(context.config.as_deref(), Some(config.to_string().as_str()));
}

#[tokio::test]
async fn linked_users_are_listed_for_the_instance() {
    let dir = tempfile::tempdir().unwrap();
    let (database, service) = service(dir.path()).await;
    let instance = Uuid::now_v7();
    let other = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    seed_instance(&database, instance).await;
    seed_instance(&database, other).await;
    seed_user(&database, first).await;
    seed_user(&database, second).await;

    service
        .put(
            instance,
            first,
            Some("one".to_owned()),
            &json!({"token": "1"}),
        )
        .await
        .unwrap();
    service
        .put(
            instance,
            second,
            Some("two".to_owned()),
            &json!({"token": "2"}),
        )
        .await
        .unwrap();
    service
        .put(
            other,
            first,
            Some("elsewhere".to_owned()),
            &json!({"token": "x"}),
        )
        .await
        .unwrap();

    let mut users = service.linked_users(instance).await.unwrap();
    users.sort_by_key(|user| user.user_id);
    assert_eq!(users.len(), 2);
    let mut expected = [first, second];
    expected.sort();
    assert_eq!(users[0].user_id, expected[0]);
    assert_eq!(users[1].user_id, expected[1]);
    assert_eq!(users[0].link.account_name.as_deref(), Some("one"));
    assert_eq!(users[1].link.config, json!({"token": "2"}));
    assert_eq!(
        users[0].link.context(users[0].user_id).user_id,
        users[0].user_id.to_string()
    );
}

#[tokio::test]
async fn a_second_write_replaces_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let (database, service) = service(dir.path()).await;
    let instance = Uuid::now_v7();
    let user = Uuid::now_v7();
    seed_instance(&database, instance).await;
    seed_user(&database, user).await;

    service
        .put(
            instance,
            user,
            Some("old".to_owned()),
            &json!({"token": "a"}),
        )
        .await
        .unwrap();
    service
        .put(
            instance,
            user,
            Some("new".to_owned()),
            &json!({"token": "b"}),
        )
        .await
        .unwrap();

    let link = service.get(instance, user).await.unwrap().unwrap();
    assert_eq!(link.account_name.as_deref(), Some("new"));
    assert_eq!(link.config, json!({"token": "b"}));
    assert_eq!(service.linked_users(instance).await.unwrap().len(), 1);
}

#[tokio::test]
async fn delete_removes_the_link_and_reports_whether_one_existed() {
    let dir = tempfile::tempdir().unwrap();
    let (database, service) = service(dir.path()).await;
    let instance = Uuid::now_v7();
    let user = Uuid::now_v7();
    seed_instance(&database, instance).await;
    seed_user(&database, user).await;

    service
        .put(instance, user, None, &json!({"token": "a"}))
        .await
        .unwrap();
    assert!(service.delete(instance, user).await.unwrap());
    assert!(service.get(instance, user).await.unwrap().is_none());
    assert!(!service.delete(instance, user).await.unwrap());
}

#[tokio::test]
async fn a_configuration_that_violates_the_schema_is_rejected() {
    let schema = json!({
        "type": "object",
        "required": ["token"],
        "properties": {"token": {"type": "string"}}
    });

    let error = validate_user_config(&schema, &json!({})).unwrap_err();
    match error {
        UserConfigError::Schema(problems) => {
            assert!(problems[0].contains("token"), "{problems:?}");
        }
        other => panic!("expected schema problems, got {other}"),
    }
}

#[tokio::test]
async fn defaults_are_filled_in_before_the_plugin_is_called() {
    let schema = json!({
        "type": "object",
        "properties": {
            "token": {"type": "string"},
            "mode": {"type": "string", "default": "full"}
        }
    });

    let config = validate_user_config(&schema, &json!({"token": "good"})).unwrap();
    assert_eq!(config, json!({"token": "good", "mode": "full"}));
}

/// A listing masks every write-only field and leaves the rest alone.
#[test]
fn a_listing_masks_every_write_only_field() {
    let schema = json!({
        "type": "object",
        "properties": {
            "token": { "type": "string", "writeOnly": true },
            "username": { "type": "string" },
        },
    });
    let redacted = redact_user_config(&schema, &json!({ "token": "abc", "username": "echo" }));
    assert_eq!(redacted, json!({ "token": "[sealed]", "username": "echo" }));

    let absent = redact_user_config(&schema, &json!({ "username": "echo" }));
    assert_eq!(
        absent,
        json!({ "username": "echo" }),
        "a field the configuration does not carry is not invented"
    );
}

/// A write that echoes the placeholder keeps the stored secret and applies the
/// rest of the form.
#[test]
fn an_echoed_placeholder_keeps_the_stored_value() {
    let schema = json!({
        "type": "object",
        "properties": {
            "token": { "type": "string", "writeOnly": true },
            "username": { "type": "string" },
        },
    });
    let merged = merge_user_config(
        &schema,
        &json!({ "token": "[sealed]", "username": "new" }),
        Some(&json!({ "token": "abc", "username": "old" })),
    )
    .unwrap();
    assert_eq!(merged, json!({ "token": "abc", "username": "new" }));
}

/// A placeholder with nothing stored under it is refused, naming the field.
#[test]
fn a_placeholder_with_nothing_stored_is_refused() {
    let schema = json!({
        "type": "object",
        "properties": { "token": { "type": "string", "writeOnly": true } },
    });
    let error = merge_user_config(&schema, &json!({ "token": "[sealed]" }), None).unwrap_err();
    match error {
        UserConfigError::Config(message) => assert!(
            message.contains("/token"),
            "the refusal names the field: {message}"
        ),
        other => panic!("expected a configuration error, got {other}"),
    }
}
