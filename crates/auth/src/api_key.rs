//! API keys.
//!
//! An API key is the credential modern clients use on both surfaces: as
//! `apiKey=` on `/rest` (the OpenSubsonic `apiKeyAuthentication` extension) and
//! as `Authorization: Bearer` on `/api/v1`. A key looks like
//! `hs_` followed by 52 lowercase RFC 4648 base32 characters, which is the
//! encoding of 32 random bytes - 256 bits of entropy, still short enough to fit
//! in a query parameter.
//!
//! The plaintext is returned once, by [`ApiKeys::generate`], and never stored:
//! the row keeps the SHA-256 of the full key in `key_hash`, which is what
//! [`ApiKeys::resolve`] looks up, plus the leading characters in `key_prefix` so
//! a listing can name a key without holding the secret. Revoking a key stamps
//! `revoked_at`, and a revoked key, a key of a disabled account, and a key that
//! was never issued all fail resolution the same way.

use hearthsound_core::new_uuid_v7;
use hearthsound_db::Database;
use rand::RngExt;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::AuthError;

/// The literal every API key starts with.
pub const API_KEY_PREFIX: &str = "hs_";

/// How many random bytes back a key.
const KEY_BYTES: usize = 32;

/// How many base32 characters encode [`KEY_BYTES`].
const PAYLOAD_LEN: usize = 52;

/// How many payload characters the stored `key_prefix` keeps.
const PREFIX_LEN: usize = 8;

/// The lowercase RFC 4648 base32 alphabet a key payload uses.
const PAYLOAD_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

/// A freshly generated API key.
///
/// The plaintext is the only copy the server ever has; it is shown to the user
/// once and cannot be recovered afterwards. The prefix identifies the key in a
/// later listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedApiKey {
    /// The full key to hand to the client, `hs_<52 base32 characters>`.
    pub plaintext: String,
    /// The key's leading characters, as stored in `key_prefix`.
    pub key_prefix: String,
}

/// An API key as a listing shows it.
///
/// The plaintext and its SHA-256 are deliberately absent: a listing answers
/// "which keys exist for this user", not "what are they".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    /// The key's UUIDv7 primary key.
    pub id: Uuid,
    /// The account the key belongs to.
    pub user_id: Uuid,
    /// The name the user gave the key, for example a client name.
    pub name: String,
    /// The key's leading characters.
    pub key_prefix: String,
    /// When the key was created, in unix milliseconds.
    pub created_at: i64,
    /// When the key last authenticated a request, in unix milliseconds.
    pub last_used_at: Option<i64>,
    /// When the key was revoked, in unix milliseconds, or `None` while active.
    pub revoked_at: Option<i64>,
}

impl ApiKey {
    /// Reports whether the key has been revoked.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn example(key: &hearthsound_auth::ApiKey) -> bool {
    /// key.revoked_at.is_some()
    /// # }
    /// ```
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// The user a resolved API key belongs to.
///
/// This is what authentication needs and nothing else: who the request is, and
/// which key proved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyOwner {
    /// The id of the key that matched, for `last_used_at` bookkeeping and for
    /// naming the credential a principal used.
    pub key_id: Uuid,
    /// The owner's account id.
    pub user_id: Uuid,
    /// The owner's login name.
    pub username: String,
    /// Whether the owner is an administrator.
    pub is_admin: bool,
    /// Whether the owner may request acquisitions.
    pub can_acquire: bool,
    /// Whether the owner may upload files.
    pub can_upload: bool,
    /// Whether the owner may download stored files.
    pub can_download: bool,
}

/// The `api_keys` table.
///
/// Cloning is cheap: every clone shares the same two pools.
#[derive(Debug, Clone)]
pub struct ApiKeys {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

/// One joined row of `api_keys` and `users`.
#[derive(sqlx::FromRow)]
struct OwnerRow {
    key_id: Vec<u8>,
    user_id: Vec<u8>,
    username: String,
    is_admin: i64,
    can_acquire: i64,
    can_upload: i64,
    can_download: i64,
}

/// One row of the `api_keys` table.
#[derive(sqlx::FromRow)]
struct ApiKeyRow {
    id: Vec<u8>,
    user_id: Vec<u8>,
    name: String,
    key_prefix: String,
    created_at: i64,
    last_used_at: Option<i64>,
    revoked_at: Option<i64>,
}

impl ApiKeyRow {
    /// Converts a row into a listing entry.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::CorruptId`] when a key is not 16 bytes.
    fn into_api_key(self) -> Result<ApiKey, AuthError> {
        Ok(ApiKey {
            id: Uuid::from_slice(&self.id)?,
            user_id: Uuid::from_slice(&self.user_id)?,
            name: self.name,
            key_prefix: self.key_prefix,
            created_at: self.created_at,
            last_used_at: self.last_used_at,
            revoked_at: self.revoked_at,
        })
    }
}

impl ApiKeys {
    /// Builds the repository over a database.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    ///
    /// # Returns
    ///
    /// The repository.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(db: &hearthsound_db::Database) {
    /// let keys = hearthsound_auth::ApiKeys::new(db);
    /// # let _ = keys;
    /// # }
    /// ```
    pub fn new(db: &Database) -> ApiKeys {
        ApiKeys {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Generates and stores a key for an account.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the key belongs to. The column is a foreign
    ///   key, so an unknown user is a database error.
    /// - `name`: the name the user gives the key, for example a client name.
    ///
    /// # Returns
    ///
    /// The plaintext to hand to the client and the prefix the row stores. The
    /// plaintext is not recoverable afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the insert fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(keys: &hearthsound_auth::ApiKeys, user_id: uuid::Uuid) -> Result<(), hearthsound_auth::AuthError> {
    /// let generated = keys.generate(user_id, "Symfonium").await?;
    /// assert!(generated.plaintext.starts_with("hs_"));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn generate(&self, user_id: Uuid, name: &str) -> Result<GeneratedApiKey, AuthError> {
        let mut bytes = [0u8; KEY_BYTES];
        rand::rng().fill(&mut bytes[..]);

        let payload = data_encoding::BASE32_NOPAD
            .encode(&bytes)
            .to_ascii_lowercase();
        let plaintext = format!("{API_KEY_PREFIX}{payload}");
        let key_prefix = format!("{API_KEY_PREFIX}{}", &payload[..PREFIX_LEN]);
        let key_hash = Sha256::digest(plaintext.as_bytes());

        let id = new_uuid_v7();
        sqlx::query!(
            "INSERT INTO api_keys (id, user_id, name, key_prefix, key_hash, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            id.as_bytes().as_slice(),
            user_id.as_bytes().as_slice(),
            name,
            key_prefix,
            key_hash.as_slice(),
            crate::now_millis(),
        )
        .execute(&self.writer)
        .await?;

        Ok(GeneratedApiKey {
            plaintext,
            key_prefix,
        })
    }

    /// Revokes a key.
    ///
    /// The row is kept so a listing can still show the key. Revoking a key that
    /// is already revoked changes nothing.
    ///
    /// # Parameters
    ///
    /// - `id`: the key to revoke.
    ///
    /// # Returns
    ///
    /// `true` when this call revoked the key, `false` when it was already
    /// revoked or does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the update fails.
    pub async fn revoke(&self, id: Uuid) -> Result<bool, AuthError> {
        let affected = sqlx::query!(
            "UPDATE api_keys SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
            crate::now_millis(),
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Lists an account's keys, newest first.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account whose keys to list.
    ///
    /// # Returns
    ///
    /// Every key of the account, revoked ones included, ordered by creation
    /// time with the id as a tie breaker.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    pub async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<ApiKey>, AuthError> {
        let rows = sqlx::query_as!(
            ApiKeyRow,
            "SELECT id, user_id, name, key_prefix, created_at, last_used_at, revoked_at
             FROM api_keys WHERE user_id = ? ORDER BY created_at DESC, id",
            user_id.as_bytes().as_slice(),
        )
        .fetch_all(&self.readers)
        .await?;
        rows.into_iter().map(ApiKeyRow::into_api_key).collect()
    }

    /// Resolves a presented key to its owner.
    ///
    /// A key that is malformed (wrong prefix, length, or alphabet), unknown,
    /// revoked, or owned by a disabled account resolves to `None`. All four
    /// cases are deliberately indistinguishable, so a caller cannot learn
    /// whether a key once existed.
    ///
    /// # Parameters
    ///
    /// - `key`: the presented key, exactly as the client sent it.
    ///
    /// # Returns
    ///
    /// The key's id and its owner's identity, or `None` when the key does not
    /// authenticate.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    pub async fn resolve(&self, key: &str) -> Result<Option<ApiKeyOwner>, AuthError> {
        if !is_well_formed(key) {
            return Ok(None);
        }
        let hash = Sha256::digest(key.as_bytes());
        let row = sqlx::query_as!(
            OwnerRow,
            "SELECT k.id AS key_id, u.id AS user_id, u.username, u.is_admin,
                    u.can_acquire, u.can_upload, u.can_download
             FROM api_keys k JOIN users u ON u.id = k.user_id
             WHERE k.key_hash = ? AND k.revoked_at IS NULL AND u.disabled_at IS NULL",
            hash.as_slice(),
        )
        .fetch_optional(&self.readers)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(ApiKeyOwner {
            key_id: Uuid::from_slice(&row.key_id)?,
            user_id: Uuid::from_slice(&row.user_id)?,
            username: row.username,
            is_admin: row.is_admin != 0,
            can_acquire: row.can_acquire != 0,
            can_upload: row.can_upload != 0,
            can_download: row.can_download != 0,
        }))
    }

    /// Records that a key just authenticated a request.
    ///
    /// # Parameters
    ///
    /// - `id`: the key that matched.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the update fails.
    pub async fn touch_last_used(&self, id: Uuid) -> Result<(), AuthError> {
        sqlx::query!(
            "UPDATE api_keys SET last_used_at = ? WHERE id = ?",
            crate::now_millis(),
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }
}

/// Reports whether a string has the shape of an API key.
///
/// # Parameters
///
/// - `key`: the candidate key.
///
/// # Returns
///
/// `true` when the string is `hs_` followed by exactly 52 characters of the
/// lowercase RFC 4648 base32 alphabet.
fn is_well_formed(key: &str) -> bool {
    let Some(payload) = key.strip_prefix(API_KEY_PREFIX) else {
        return false;
    };
    payload.len() == PAYLOAD_LEN && payload.bytes().all(|byte| PAYLOAD_ALPHABET.contains(&byte))
}

#[cfg(test)]
mod tests {
    use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
    use hearthsound_db::{Database, MIGRATOR};

    use super::*;
    use crate::{NewUser, Users};

    /// Opens a migrated temporary database with one account.
    async fn database() -> (tempfile::TempDir, Database, Uuid) {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        let snapshot =
            SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default())
                .unwrap();
        let user = Users::new(&database)
            .create(&snapshot, &NewUser::new("echo", "Echo", "hunter2", false))
            .await
            .unwrap();
        (dir, database, user.id)
    }

    #[tokio::test]
    async fn generates_a_key_of_the_documented_shape() {
        let (_dir, db, user_id) = database().await;
        let keys = ApiKeys::new(&db);

        let generated = keys.generate(user_id, "Symfonium").await.unwrap();

        assert!(generated.plaintext.starts_with("hs_"));
        assert_eq!(generated.plaintext.len(), API_KEY_PREFIX.len() + 52);
        assert!(is_well_formed(&generated.plaintext));
        assert_eq!(generated.key_prefix.len(), API_KEY_PREFIX.len() + 8);
        assert!(generated.plaintext.starts_with(&generated.key_prefix));

        let stored = keys.list_for_user(user_id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "Symfonium");
        assert_eq!(stored[0].key_prefix, generated.key_prefix);
        assert!(!stored[0].is_revoked());
        assert!(stored[0].last_used_at.is_none());
    }

    #[tokio::test]
    async fn generates_distinct_keys() {
        let (_dir, db, user_id) = database().await;
        let keys = ApiKeys::new(&db);

        let first = keys.generate(user_id, "one").await.unwrap();
        let second = keys.generate(user_id, "two").await.unwrap();
        assert_ne!(first.plaintext, second.plaintext);
    }

    #[tokio::test]
    async fn resolves_a_key_to_its_owner() {
        let (_dir, db, user_id) = database().await;
        let keys = ApiKeys::new(&db);
        let generated = keys.generate(user_id, "Symfonium").await.unwrap();

        let owner = keys.resolve(&generated.plaintext).await.unwrap().unwrap();
        assert_eq!(owner.user_id, user_id);
        assert_eq!(owner.username, "echo");
        assert!(!owner.is_admin);
        assert!(owner.can_acquire && owner.can_upload && owner.can_download);
        assert_eq!(
            owner.key_id,
            keys.list_for_user(user_id).await.unwrap()[0].id
        );
    }

    #[tokio::test]
    async fn unknown_and_malformed_keys_do_not_resolve() {
        let (_dir, db, user_id) = database().await;
        let keys = ApiKeys::new(&db);
        let generated = keys.generate(user_id, "Symfonium").await.unwrap();

        for candidate in [
            "hs_0000000000000000000000000000000000000000000000000000",
            "hs_short",
            "nope_0000000000000000000000000000000000000000000000000000",
            "HS_0000000000000000000000000000000000000000000000000000",
            "",
        ] {
            assert!(
                keys.resolve(candidate).await.unwrap().is_none(),
                "`{candidate}` must not resolve"
            );
        }

        let mut tampered = generated.plaintext.clone().into_bytes();
        tampered[10] = if tampered[10] == b'a' { b'b' } else { b'a' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(keys.resolve(&tampered).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn revoked_keys_do_not_resolve() {
        let (_dir, db, user_id) = database().await;
        let keys = ApiKeys::new(&db);
        let generated = keys.generate(user_id, "Symfonium").await.unwrap();
        let key_id = keys.list_for_user(user_id).await.unwrap()[0].id;

        assert!(keys.revoke(key_id).await.unwrap());
        assert!(!keys.revoke(key_id).await.unwrap());

        assert!(keys.resolve(&generated.plaintext).await.unwrap().is_none());
        let listed = &keys.list_for_user(user_id).await.unwrap()[0];
        assert!(listed.is_revoked());
    }

    #[tokio::test]
    async fn keys_of_disabled_users_do_not_resolve() {
        let (_dir, db, user_id) = database().await;
        let keys = ApiKeys::new(&db);
        let generated = keys.generate(user_id, "Symfonium").await.unwrap();

        assert!(keys.resolve(&generated.plaintext).await.unwrap().is_some());
        assert!(Users::new(&db).disable(user_id).await.unwrap());
        assert!(keys.resolve(&generated.plaintext).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn records_the_last_use_timestamp() {
        let (_dir, db, user_id) = database().await;
        let keys = ApiKeys::new(&db);
        keys.generate(user_id, "Symfonium").await.unwrap();
        let key_id = keys.list_for_user(user_id).await.unwrap()[0].id;

        keys.touch_last_used(key_id).await.unwrap();

        let listed = &keys.list_for_user(user_id).await.unwrap()[0];
        assert!(listed.last_used_at.is_some());
    }

    #[test]
    fn key_shape_is_enforced() {
        let alphabet = "abcdefghijklmnopqrstuvwxyz234567";
        let payload: String = alphabet.repeat(2).chars().take(52).collect();
        assert!(is_well_formed(&format!("hs_{payload}")));
        assert!(!is_well_formed(&format!("hs_{}", &payload[..51])));
        assert!(!is_well_formed("hs_"));
        assert!(!is_well_formed("hs_0189"));
    }
}
