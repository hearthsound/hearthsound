//! App passwords.
//!
//! A third-party Subsonic client that does not support `apiKey` authenticates
//! with `u` + `p` or with `u` + `t` + `s`, where `t = md5(password + salt)`.
//! Both schemes need a plaintext secret at request time, so the user creates one
//! app password per client instead of handing over the account password, and the
//! server has to store it reversibly. The secret is sealed with the
//! AES-256-GCM secret box ([`hearthsound_config::secret::SecretBox`]) into
//! `app_passwords.ciphertext` with its 96-bit nonce in `app_passwords.nonce`,
//! and only [`AppPasswords::active_secrets`] opens it again, for verification.
//!
//! A generated app password is 20 characters drawn uniformly from the
//! 32-symbol alphabet `0123456789abcdefghjkmnpqrstvwxyz` - 100 bits of entropy.
//! The alphabet leaves out `i`, `l`, `o` and `u`, so a secret that has to be
//! typed by hand from a screen cannot be misread.
//!
//! The plaintext is returned once, by [`AppPasswords::generate`], and never
//! rendered by `Debug`.

use std::fmt;

use hearthsound_config::secret::SecretBox;
use hearthsound_core::new_uuid_v7;
use hearthsound_db::Database;
use rand::RngExt;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::AuthError;

/// The alphabet an app password is drawn from.
///
/// 32 symbols, so a uniformly random byte's low five bits select one without
/// rejection sampling. The letters `i`, `l`, `o` and `u` are absent.
pub const APP_PASSWORD_ALPHABET: &str = "0123456789abcdefghjkmnpqrstvwxyz";

/// The number of characters in a generated app password.
///
/// 20 characters at 5 bits each is 100 bits of entropy.
pub const APP_PASSWORD_LENGTH: usize = 20;

/// A freshly generated app password.
///
/// The plaintext is the only copy the server ever has in the clear; it is shown
/// to the user once and cannot be recovered afterwards.
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedAppPassword {
    /// The app password's UUIDv7 primary key.
    pub id: Uuid,
    /// The secret to hand to the client, 20 characters of the alphabet.
    pub plaintext: String,
}

impl fmt::Debug for GeneratedAppPassword {
    /// Writes the id with the secret redacted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedAppPassword")
            .field("id", &self.id)
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

/// An app password as a listing shows it.
///
/// The ciphertext, its nonce, and the plaintext are deliberately absent: a
/// listing answers "which app passwords exist for this user", not "what are
/// they".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPassword {
    /// The app password's UUIDv7 primary key.
    pub id: Uuid,
    /// The account the app password belongs to.
    pub user_id: Uuid,
    /// The name the user gave it, for example a client name.
    pub name: String,
    /// When it was created, in unix milliseconds.
    pub created_at: i64,
    /// When it last authenticated a request, in unix milliseconds.
    pub last_used_at: Option<i64>,
    /// When it was revoked, in unix milliseconds, or `None` while active.
    pub revoked_at: Option<i64>,
}

impl AppPassword {
    /// Reports whether the app password has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }
}

/// One active app password, decrypted for verification.
#[derive(Clone, PartialEq, Eq)]
pub struct AppPasswordSecret {
    /// The app password's id, so a successful match can record its use.
    pub id: Uuid,
    /// The decrypted secret. It is ASCII in practice but kept as bytes because
    /// that is what both verification schemes consume.
    pub secret: Vec<u8>,
}

impl fmt::Debug for AppPasswordSecret {
    /// Writes the id with the secret redacted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppPasswordSecret")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// The `app_passwords` table.
///
/// Cloning is cheap: every clone shares the same two pools and secret box. The
/// secret box redacts its own key material, so `Debug` cannot leak it.
#[derive(Debug, Clone)]
pub struct AppPasswords {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
    /// The box that seals and opens every secret.
    secret_box: SecretBox,
}

/// One row of the `app_passwords` table.
#[derive(sqlx::FromRow)]
struct AppPasswordRow {
    id: Vec<u8>,
    user_id: Vec<u8>,
    name: String,
    created_at: i64,
    last_used_at: Option<i64>,
    revoked_at: Option<i64>,
}

/// One row of the `app_passwords` table with its sealed secret.
#[derive(sqlx::FromRow)]
struct SealedSecretRow {
    id: Vec<u8>,
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
}

impl AppPasswordRow {
    /// Converts a row into a listing entry.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::CorruptId`] when an id is not 16 bytes.
    fn into_app_password(self) -> Result<AppPassword, AuthError> {
        Ok(AppPassword {
            id: Uuid::from_slice(&self.id)?,
            user_id: Uuid::from_slice(&self.user_id)?,
            name: self.name,
            created_at: self.created_at,
            last_used_at: self.last_used_at,
            revoked_at: self.revoked_at,
        })
    }
}

impl AppPasswords {
    /// Builds the repository over a database and a secret box.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    /// - `secret_box`: the AES-256-GCM box every app password is sealed with.
    ///
    /// # Returns
    ///
    /// The repository.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(db: &hearthsound_db::Database, secret_box: hearthsound_config::secret::SecretBox) {
    /// let app_passwords = hearthsound_auth::AppPasswords::new(db, secret_box);
    /// # let _ = app_passwords;
    /// # }
    /// ```
    pub fn new(db: &Database, secret_box: SecretBox) -> AppPasswords {
        AppPasswords {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            secret_box,
        }
    }

    /// Generates and stores an app password for an account.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the app password belongs to. The column is a
    ///   foreign key, so an unknown user is a database error.
    /// - `name`: the name the user gives it, for example a client name.
    ///
    /// # Returns
    ///
    /// The id and the plaintext to hand to the client. The plaintext is not
    /// recoverable through the API afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Secret`] when the secret box refuses to seal the
    /// value and [`AuthError::Database`] when the insert fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(app_passwords: &hearthsound_auth::AppPasswords, user_id: uuid::Uuid) -> Result<(), hearthsound_auth::AuthError> {
    /// let generated = app_passwords.generate(user_id, "Amperfy").await?;
    /// assert_eq!(generated.plaintext.len(), hearthsound_auth::APP_PASSWORD_LENGTH);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn generate(
        &self,
        user_id: Uuid,
        name: &str,
    ) -> Result<GeneratedAppPassword, AuthError> {
        let plaintext = random_secret();
        let (ciphertext, nonce) = self.secret_box.seal(plaintext.as_bytes())?;

        let id = new_uuid_v7();
        sqlx::query!(
            "INSERT INTO app_passwords (id, user_id, name, ciphertext, nonce, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            id.as_bytes().as_slice(),
            user_id.as_bytes().as_slice(),
            name,
            ciphertext.as_slice(),
            nonce.as_slice(),
            crate::now_millis(),
        )
        .execute(&self.writer)
        .await?;

        Ok(GeneratedAppPassword { id, plaintext })
    }

    /// Revokes an app password.
    ///
    /// The row is kept so a listing can still show it, and a client holding the
    /// revoked secret fails authentication immediately. Revoking one that is
    /// already revoked changes nothing.
    ///
    /// # Parameters
    ///
    /// - `id`: the app password to revoke.
    ///
    /// # Returns
    ///
    /// `true` when this call revoked it, `false` when it was already revoked or
    /// does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the update fails.
    pub async fn revoke(&self, id: Uuid) -> Result<bool, AuthError> {
        let affected = sqlx::query!(
            "UPDATE app_passwords SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
            crate::now_millis(),
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Lists an account's app passwords, newest first.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account whose app passwords to list.
    ///
    /// # Returns
    ///
    /// Every app password of the account, revoked ones included, ordered by
    /// creation time with the id as a tie breaker.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    pub async fn list_for_user(&self, user_id: Uuid) -> Result<Vec<AppPassword>, AuthError> {
        let rows = sqlx::query_as!(
            AppPasswordRow,
            "SELECT id, user_id, name, created_at, last_used_at, revoked_at
             FROM app_passwords WHERE user_id = ? ORDER BY created_at DESC, id",
            user_id.as_bytes().as_slice(),
        )
        .fetch_all(&self.readers)
        .await?;
        rows.into_iter()
            .map(AppPasswordRow::into_app_password)
            .collect()
    }

    /// Opens every active app password of an account.
    ///
    /// Verification needs the plaintext secret of each candidate, because the
    /// Subsonic schemes hash or compare it. Only rows with no `revoked_at` are
    /// returned.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account whose app passwords to open.
    ///
    /// # Returns
    ///
    /// One entry per active app password, ordered by creation time.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Secret`] when a stored row cannot be opened, which
    /// means the row was written under a different key or has been tampered
    /// with, and [`AuthError::Database`] when the query fails.
    pub async fn active_secrets(&self, user_id: Uuid) -> Result<Vec<AppPasswordSecret>, AuthError> {
        let rows = sqlx::query_as!(
            SealedSecretRow,
            "SELECT id, ciphertext, nonce FROM app_passwords
             WHERE user_id = ? AND revoked_at IS NULL ORDER BY created_at",
            user_id.as_bytes().as_slice(),
        )
        .fetch_all(&self.readers)
        .await?;

        let mut secrets = Vec::with_capacity(rows.len());
        for row in rows {
            let secret = self.secret_box.open(&row.ciphertext, &row.nonce)?;
            secrets.push(AppPasswordSecret {
                id: Uuid::from_slice(&row.id)?,
                secret,
            });
        }
        Ok(secrets)
    }

    /// Records that an app password just authenticated a request.
    ///
    /// # Parameters
    ///
    /// - `id`: the app password that matched.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the update fails.
    pub async fn touch_last_used(&self, id: Uuid) -> Result<(), AuthError> {
        sqlx::query!(
            "UPDATE app_passwords SET last_used_at = ? WHERE id = ?",
            crate::now_millis(),
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }
}

/// Generates a random app password secret.
///
/// Each character comes from one random byte's low five bits, which is uniform
/// because 32 divides 256, so no rejection sampling is needed.
///
/// # Returns
///
/// [`APP_PASSWORD_LENGTH`] characters of [`APP_PASSWORD_ALPHABET`].
fn random_secret() -> String {
    let mut bytes = [0u8; APP_PASSWORD_LENGTH];
    rand::rng().fill(&mut bytes[..]);

    let alphabet = APP_PASSWORD_ALPHABET.as_bytes();
    let mut secret = String::with_capacity(APP_PASSWORD_LENGTH);
    for byte in bytes {
        secret.push(alphabet[usize::from(byte & 0x1f)] as char);
    }
    secret
}

#[cfg(test)]
mod tests {
    use hearthsound_config::secret::SecretKey;
    use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
    use hearthsound_db::{Database, MIGRATOR};

    use super::*;
    use crate::{NewUser, Users};

    /// Opens a migrated temporary database with one account and a fresh key.
    async fn database() -> (tempfile::TempDir, Database, Uuid, AppPasswords) {
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
        let app_passwords = AppPasswords::new(&database, SecretKey::generate().secret_box());
        (dir, database, user.id, app_passwords)
    }

    #[test]
    fn secrets_use_the_documented_alphabet_and_length() {
        let alphabet = APP_PASSWORD_ALPHABET.as_bytes();
        for _ in 0..64 {
            let secret = random_secret();
            assert_eq!(secret.len(), APP_PASSWORD_LENGTH);
            assert!(secret.bytes().all(|byte| alphabet.contains(&byte)));
        }
    }

    #[test]
    fn secrets_are_distinct() {
        let first = random_secret();
        let second = random_secret();
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn generates_lists_and_opens_an_app_password() {
        let (_dir, _db, user_id, app_passwords) = database().await;

        let generated = app_passwords.generate(user_id, "Amperfy").await.unwrap();
        assert_eq!(generated.plaintext.len(), APP_PASSWORD_LENGTH);
        assert!(!format!("{generated:?}").contains(&generated.plaintext));

        let listed = app_passwords.list_for_user(user_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Amperfy");
        assert!(!listed[0].is_revoked());
        assert_eq!(listed[0].id, generated.id);

        let secrets = app_passwords.active_secrets(user_id).await.unwrap();
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].id, generated.id);
        assert_eq!(secrets[0].secret, generated.plaintext.as_bytes());
        assert!(!format!("{secrets:?}").contains(&generated.plaintext));
    }

    #[tokio::test]
    async fn a_revoked_app_password_is_not_opened() {
        let (_dir, _db, user_id, app_passwords) = database().await;
        let generated = app_passwords.generate(user_id, "Amperfy").await.unwrap();

        assert!(app_passwords.revoke(generated.id).await.unwrap());
        assert!(!app_passwords.revoke(generated.id).await.unwrap());

        assert!(
            app_passwords
                .active_secrets(user_id)
                .await
                .unwrap()
                .is_empty()
        );
        let listed = &app_passwords.list_for_user(user_id).await.unwrap()[0];
        assert!(listed.is_revoked());
    }

    #[tokio::test]
    async fn opens_every_active_secret_of_an_account() {
        let (_dir, _db, user_id, app_passwords) = database().await;
        let first = app_passwords.generate(user_id, "one").await.unwrap();
        let second = app_passwords.generate(user_id, "two").await.unwrap();

        let secrets = app_passwords.active_secrets(user_id).await.unwrap();
        let plaintexts: Vec<Vec<u8>> = secrets.iter().map(|entry| entry.secret.clone()).collect();
        assert_eq!(plaintexts.len(), 2);
        assert!(plaintexts.contains(&first.plaintext.as_bytes().to_vec()));
        assert!(plaintexts.contains(&second.plaintext.as_bytes().to_vec()));
    }

    #[tokio::test]
    async fn secrets_are_stored_sealed_and_not_in_the_clear() {
        let (_dir, db, user_id, app_passwords) = database().await;
        let generated = app_passwords.generate(user_id, "Amperfy").await.unwrap();

        let stored = sqlx::query!(
            "SELECT ciphertext, nonce FROM app_passwords WHERE user_id = ?",
            user_id.as_bytes().as_slice(),
        )
        .fetch_one(db.readers())
        .await
        .unwrap();

        assert_eq!(stored.nonce.len(), 12);
        assert_ne!(stored.ciphertext, generated.plaintext.as_bytes());
        assert!(
            !stored
                .ciphertext
                .windows(generated.plaintext.len())
                .any(|window| window == generated.plaintext.as_bytes()),
            "the ciphertext must not contain the plaintext"
        );
    }

    #[tokio::test]
    async fn a_secret_sealed_under_another_key_is_rejected() {
        let (_dir, db, user_id, app_passwords) = database().await;
        app_passwords.generate(user_id, "Amperfy").await.unwrap();
        drop(app_passwords);

        // Reopen the same database under a different key.
        let other = AppPasswords::new(&db, SecretKey::generate().secret_box());
        let error = other.active_secrets(user_id).await.unwrap_err();
        assert!(matches!(error, AuthError::Secret(_)));
    }

    #[tokio::test]
    async fn records_the_last_use_timestamp() {
        let (_dir, _db, user_id, app_passwords) = database().await;
        let generated = app_passwords.generate(user_id, "Amperfy").await.unwrap();

        app_passwords.touch_last_used(generated.id).await.unwrap();

        let listed = &app_passwords.list_for_user(user_id).await.unwrap()[0];
        assert!(listed.last_used_at.is_some());
    }
}
