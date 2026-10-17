//! User accounts and their permissions.
//!
//! A user is the root of everything personal in the server. This module owns the
//! `users` table: creating an account with an Argon2id password hash, reading
//! accounts back by id or by username, disabling them without deleting their
//! data, and listing them. The three permission flags a Subsonic client cares
//! about - acquire, upload and download - live directly on the row and default
//! from the `users.default_permissions` setting when an account is created.

use std::fmt;
use std::str::FromStr;

use hearthsound_config::{SettingsStore, keys};
use hearthsound_core::new_uuid_v7;
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::AuthError;
use crate::password;

/// One permission a user may hold.
///
/// The names match the entries of the `users.default_permissions` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// May request acquisition of tracks that are not on the server.
    Acquire,
    /// May upload files into the library.
    Upload,
    /// May download stored files.
    Download,
}

impl Permission {
    /// Every permission, in a fixed order.
    pub const ALL: [Permission; 3] = [
        Permission::Acquire,
        Permission::Upload,
        Permission::Download,
    ];

    /// Returns the name this permission is written as.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_auth::Permission;
    ///
    /// assert_eq!(Permission::Acquire.as_str(), "acquire");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            Permission::Acquire => "acquire",
            Permission::Upload => "upload",
            Permission::Download => "download",
        }
    }
}

impl fmt::Display for Permission {
    /// Writes the permission name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Permission {
    type Err = UnknownPermission;

    /// Parses a permission name.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownPermission`] for any other name.
    fn from_str(name: &str) -> Result<Permission, UnknownPermission> {
        Permission::ALL
            .into_iter()
            .find(|permission| permission.as_str() == name)
            .ok_or_else(|| UnknownPermission {
                name: name.to_owned(),
            })
    }
}

/// The error [`Permission`] parsing returns for an unknown name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{name}` is not a known permission")]
pub struct UnknownPermission {
    /// The unrecognized name.
    pub name: String,
}

/// The three permission flags of a user, as booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserPermissions {
    /// May request acquisitions.
    pub acquire: bool,
    /// May upload files.
    pub upload: bool,
    /// May download stored files.
    pub download: bool,
}

impl UserPermissions {
    /// Builds a flag set from permission names, ignoring unknown ones.
    ///
    /// # Parameters
    ///
    /// - `names`: the names to grant, for example the value of
    ///   `users.default_permissions`.
    ///
    /// # Returns
    ///
    /// The matching flags; every flag whose name is absent is `false`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_auth::UserPermissions;
    ///
    /// let granted = UserPermissions::from_names(&["acquire".to_owned()]);
    /// assert!(granted.acquire);
    /// assert!(!granted.upload);
    /// assert!(!granted.download);
    /// ```
    pub fn from_names(names: &[String]) -> UserPermissions {
        let holds = |permission: Permission| names.iter().any(|name| name == permission.as_str());
        UserPermissions {
            acquire: holds(Permission::Acquire),
            upload: holds(Permission::Upload),
            download: holds(Permission::Download),
        }
    }

    /// Reports whether a permission is granted.
    ///
    /// # Parameters
    ///
    /// - `permission`: the permission to check.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_auth::{Permission, UserPermissions};
    ///
    /// let granted = UserPermissions { acquire: true, upload: false, download: false };
    /// assert!(granted.allows(Permission::Acquire));
    /// assert!(!granted.allows(Permission::Upload));
    /// ```
    pub fn allows(&self, permission: Permission) -> bool {
        match permission {
            Permission::Acquire => self.acquire,
            Permission::Upload => self.upload,
            Permission::Download => self.download,
        }
    }
}

/// An account.
///
/// The `password_hash` is never rendered by `Debug` and never returned to a
/// client; only [`crate::verify_password`] sees it.
#[derive(Clone, PartialEq, Eq)]
pub struct User {
    /// The account's UUIDv7 primary key.
    pub id: Uuid,
    /// The login name, unique case-insensitively.
    pub username: String,
    /// The name shown to people.
    pub display_name: String,
    /// The Argon2id hash of the account password.
    password_hash: String,
    /// Whether the account is an administrator.
    pub is_admin: bool,
    /// Whether the account may request acquisitions.
    pub can_acquire: bool,
    /// Whether the account may upload files.
    pub can_upload: bool,
    /// Whether the account may download stored files.
    pub can_download: bool,
    /// When the account was created, in unix milliseconds.
    pub created_at: i64,
    /// When the account was last changed, in unix milliseconds.
    pub updated_at: i64,
    /// When the account was disabled, in unix milliseconds, or `None`.
    pub disabled_at: Option<i64>,
}

impl User {
    /// Reports whether the account is disabled.
    ///
    /// # Returns
    ///
    /// `true` when `disabled_at` is set; a disabled account fails
    /// authentication exactly like an unknown one.
    pub fn is_disabled(&self) -> bool {
        self.disabled_at.is_some()
    }

    /// Returns the account's permission flags.
    pub fn permissions(&self) -> UserPermissions {
        UserPermissions {
            acquire: self.can_acquire,
            upload: self.can_upload,
            download: self.can_download,
        }
    }

    /// Returns the stored Argon2id hash of the account password.
    ///
    /// # Returns
    ///
    /// The `password_hash` column. It is kept out of the public fields and out
    /// of `Debug` so it cannot leak into a response or a log line by accident.
    pub fn password_hash(&self) -> &str {
        &self.password_hash
    }
}

impl fmt::Debug for User {
    /// Writes the account with the password hash redacted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("display_name", &self.display_name)
            .field("password_hash", &"<redacted>")
            .field("is_admin", &self.is_admin)
            .field("can_acquire", &self.can_acquire)
            .field("can_upload", &self.can_upload)
            .field("can_download", &self.can_download)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("disabled_at", &self.disabled_at)
            .finish()
    }
}

/// The fields of a new account.
///
/// Built with [`NewUser::new`] and passed to [`Users::create`]. The permission
/// flags are copied from the `users.default_permissions` setting unless
/// [`NewUser::with_permissions`] overrides them.
#[derive(Debug, Clone)]
pub struct NewUser {
    /// The login name, unique case-insensitively.
    pub username: String,
    /// The name shown to people.
    pub display_name: String,
    /// The plaintext account password, hashed before it is stored.
    pub password: String,
    /// Whether the account is an administrator.
    pub is_admin: bool,
    /// The permission flags to grant, or `None` to copy the
    /// `users.default_permissions` setting.
    pub permissions: Option<UserPermissions>,
}

impl NewUser {
    /// Builds the fields of a new account.
    ///
    /// # Parameters
    ///
    /// - `username`: the login name.
    /// - `display_name`: the name shown to people; an empty or blank string
    ///   falls back to the username.
    /// - `password`: the plaintext account password.
    /// - `is_admin`: whether the account is an administrator.
    ///
    /// # Returns
    ///
    /// The new account's fields, with the permissions left to the
    /// `users.default_permissions` setting.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_auth::NewUser;
    ///
    /// let new = NewUser::new("echo", "Echo", "hunter2", false);
    /// assert_eq!(new.display_name, "Echo");
    /// assert!(new.permissions.is_none());
    /// ```
    pub fn new(
        username: impl Into<String>,
        display_name: impl Into<String>,
        password: impl Into<String>,
        is_admin: bool,
    ) -> NewUser {
        NewUser {
            username: username.into(),
            display_name: display_name.into(),
            password: password.into(),
            is_admin,
            permissions: None,
        }
    }

    /// Grants an explicit set of permissions instead of the default.
    ///
    /// # Parameters
    ///
    /// - `permissions`: the flags the new account holds. The
    ///   `users.default_permissions` setting is not read at all when this is
    ///   set, so an account can be created with no permission even while the
    ///   setting grants some.
    ///
    /// # Returns
    ///
    /// The same fields with the permissions set.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_auth::{NewUser, UserPermissions};
    ///
    /// let granted = UserPermissions { acquire: false, upload: false, download: true };
    /// let new = NewUser::new("echo", "Echo", "hunter2", false).with_permissions(granted);
    /// assert_eq!(new.permissions, Some(granted));
    /// ```
    pub fn with_permissions(mut self, permissions: UserPermissions) -> NewUser {
        self.permissions = Some(permissions);
        self
    }
}

/// The fields of an account update.
///
/// Built with [`UserUpdate::default`] and the `with_` methods, or as a struct
/// literal, and passed to [`Users::update`]. Every field is optional and an
/// absent one leaves the stored value alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserUpdate {
    /// The new display name, or `None` to keep the stored one.
    pub display_name: Option<String>,
    /// Whether the account is an administrator, or `None` to keep the stored
    /// flag.
    pub is_admin: Option<bool>,
    /// The new permission flags, or `None` to keep the stored ones.
    pub permissions: Option<UserPermissions>,
}

impl UserUpdate {
    /// Sets the display name.
    ///
    /// # Parameters
    ///
    /// - `display_name`: the name to show people; a blank string falls back to
    ///   the account's username.
    ///
    /// # Returns
    ///
    /// The same update with the display name set.
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> UserUpdate {
        self.display_name = Some(display_name.into());
        self
    }

    /// Sets whether the account is an administrator.
    ///
    /// # Parameters
    ///
    /// - `is_admin`: the new flag.
    ///
    /// # Returns
    ///
    /// The same update with the flag set.
    pub fn with_is_admin(mut self, is_admin: bool) -> UserUpdate {
        self.is_admin = Some(is_admin);
        self
    }

    /// Sets the permission flags.
    ///
    /// # Parameters
    ///
    /// - `permissions`: the new flags.
    ///
    /// # Returns
    ///
    /// The same update with the permissions set.
    pub fn with_permissions(mut self, permissions: UserPermissions) -> UserUpdate {
        self.permissions = Some(permissions);
        self
    }
}

/// The `users` table.
///
/// Cloning is cheap: every clone shares the same two pools.
#[derive(Debug, Clone)]
pub struct Users {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

/// One row of the `users` table.
///
/// The four `0`/`1` columns are read as integers because SQLite stores them as
/// `INTEGER`; [`UserRow::into_user`] turns them into booleans.
#[derive(sqlx::FromRow)]
struct UserRow {
    id: Vec<u8>,
    username: String,
    display_name: String,
    password_hash: String,
    is_admin: i64,
    can_acquire: i64,
    can_upload: i64,
    can_download: i64,
    created_at: i64,
    updated_at: i64,
    disabled_at: Option<i64>,
}

impl UserRow {
    /// Converts a row into an account.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::CorruptId`] when the primary key is not 16 bytes.
    fn into_user(self) -> Result<User, AuthError> {
        Ok(User {
            id: Uuid::from_slice(&self.id)?,
            username: self.username,
            display_name: self.display_name,
            password_hash: self.password_hash,
            is_admin: self.is_admin != 0,
            can_acquire: self.can_acquire != 0,
            can_upload: self.can_upload != 0,
            can_download: self.can_download != 0,
            created_at: self.created_at,
            updated_at: self.updated_at,
            disabled_at: self.disabled_at,
        })
    }
}

impl Users {
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
    /// let users = hearthsound_auth::Users::new(db);
    /// # let _ = users;
    /// # }
    /// ```
    pub fn new(db: &Database) -> Users {
        Users {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Creates an account.
    ///
    /// The permission flags come from [`NewUser::permissions`] when it is set,
    /// and are otherwise copied from `users.default_permissions` in `settings`.
    /// The password is hashed with Argon2id before it is stored.
    ///
    /// # Parameters
    ///
    /// - `settings`: the resolved settings snapshot the permissions default
    ///   from; it is not read when the new account carries its own flags.
    /// - `new`: the account's fields.
    ///
    /// # Returns
    ///
    /// The stored account.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::EmptyField`] when the username or the password is
    /// empty, [`AuthError::UsernameTaken`] when another account already uses the
    /// username (case-insensitively), [`AuthError::Setting`] when the stored
    /// default permissions no longer resolve, and [`AuthError::Database`] when
    /// the insert fails.
    pub async fn create(&self, settings: &SettingsStore, new: &NewUser) -> Result<User, AuthError> {
        let username = new.username.trim();
        if username.is_empty() {
            return Err(AuthError::EmptyField { field: "username" });
        }
        if new.password.is_empty() {
            return Err(AuthError::EmptyField { field: "password" });
        }
        let display_name = if new.display_name.trim().is_empty() {
            username
        } else {
            new.display_name.as_str()
        };

        if self.find_by_username(username).await?.is_some() {
            return Err(AuthError::UsernameTaken {
                username: username.to_owned(),
            });
        }

        let permissions = match new.permissions {
            Some(permissions) => permissions,
            None => {
                let granted = settings.get(keys::USERS_DEFAULT_PERMISSIONS)?;
                UserPermissions::from_names(&granted)
            }
        };
        let password_hash = password::hash_password(&new.password).await?;

        let id = new_uuid_v7();
        let now = crate::now_millis();
        let insert = sqlx::query!(
            "INSERT INTO users (
                 id, username, display_name, password_hash, is_admin,
                 can_acquire, can_upload, can_download, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id.as_bytes().as_slice(),
            username,
            display_name,
            password_hash,
            new.is_admin,
            permissions.acquire,
            permissions.upload,
            permissions.download,
            now,
            now,
        )
        .execute(&self.writer)
        .await;

        if let Err(error) = insert {
            if is_unique_violation(&error) {
                return Err(AuthError::UsernameTaken {
                    username: username.to_owned(),
                });
            }
            return Err(AuthError::Database(error));
        }

        self.find_by_id(id)
            .await?
            .ok_or_else(|| AuthError::UnknownUser { user_id: id })
    }

    /// Reads an account by its id.
    ///
    /// # Parameters
    ///
    /// - `id`: the account's UUIDv7 primary key.
    ///
    /// # Returns
    ///
    /// The account, or `None` when no row has that id.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    pub async fn find_by_id(&self, id: Uuid) -> Result<Option<User>, AuthError> {
        let row = sqlx::query_as!(
            UserRow,
            "SELECT id, username, display_name, password_hash, is_admin, can_acquire, can_upload, \
             can_download, created_at, updated_at, disabled_at FROM users WHERE id = ?",
            id.as_bytes().as_slice()
        )
        .fetch_optional(&self.readers)
        .await?;
        row.map(UserRow::into_user).transpose()
    }

    /// Reads an account by its username.
    ///
    /// The lookup is case-insensitive, matching the unique index on the column.
    ///
    /// # Parameters
    ///
    /// - `username`: the login name to look up.
    ///
    /// # Returns
    ///
    /// The account, or `None` when no row has that username.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    pub async fn find_by_username(&self, username: &str) -> Result<Option<User>, AuthError> {
        let row = sqlx::query_as!(
            UserRow,
            "SELECT id, username, display_name, password_hash, is_admin, can_acquire, can_upload, \
             can_download, created_at, updated_at, disabled_at FROM users \
             WHERE username = ? COLLATE NOCASE",
            username
        )
        .fetch_optional(&self.readers)
        .await?;
        row.map(UserRow::into_user).transpose()
    }

    /// Lists every account, ordered by username.
    ///
    /// # Returns
    ///
    /// Every account, enabled or disabled, ordered case-insensitively by
    /// username with the id as a tie breaker.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    pub async fn list(&self) -> Result<Vec<User>, AuthError> {
        let rows = sqlx::query_as!(
            UserRow,
            "SELECT id, username, display_name, password_hash, is_admin, can_acquire, can_upload, \
             can_download, created_at, updated_at, disabled_at FROM users \
             ORDER BY username COLLATE NOCASE, id"
        )
        .fetch_all(&self.readers)
        .await?;
        rows.into_iter().map(UserRow::into_user).collect()
    }

    /// Disables an account.
    ///
    /// The row is kept, so the account's data survives; a disabled account
    /// fails authentication exactly like an unknown one. Disabling an account
    /// that is already disabled changes nothing.
    ///
    /// # Parameters
    ///
    /// - `id`: the account to disable.
    ///
    /// # Returns
    ///
    /// `true` when this call disabled the account, `false` when it was already
    /// disabled or does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the update fails.
    pub async fn disable(&self, id: Uuid) -> Result<bool, AuthError> {
        let now = crate::now_millis();
        let affected = sqlx::query!(
            "UPDATE users SET disabled_at = ?, updated_at = ? WHERE id = ? AND disabled_at IS NULL",
            now,
            now,
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Changes an account's display name, admin flag, and permissions.
    ///
    /// Every field of `update` is optional: an absent one is left as it was,
    /// and the row is written once with the resulting values. The whole update
    /// runs in one write transaction, so a concurrent change cannot be lost
    /// between the read and the write.
    ///
    /// # Parameters
    ///
    /// - `id`: the account to change.
    /// - `update`: the fields to set.
    ///
    /// # Returns
    ///
    /// The account as it is stored afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::UnknownUser`] when no account has that id, and
    /// [`AuthError::Database`] when the read or the write fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use hearthsound_auth::{UserUpdate, UserPermissions, Users};
    ///
    /// # async fn example(users: &Users, id: uuid::Uuid) -> Result<(), hearthsound_auth::AuthError> {
    /// let update = UserUpdate::default()
    ///     .with_display_name("Echo")
    ///     .with_permissions(UserPermissions { acquire: false, upload: false, download: true });
    /// let user = users.update(id, &update).await?;
    /// assert!(!user.can_acquire);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn update(&self, id: Uuid, update: &UserUpdate) -> Result<User, AuthError> {
        let mut tx = self.writer.begin().await?;
        let row = sqlx::query_as!(
            UserRow,
            "SELECT id, username, display_name, password_hash, is_admin, can_acquire, can_upload, \
             can_download, created_at, updated_at, disabled_at FROM users WHERE id = ?",
            id.as_bytes().as_slice()
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AuthError::UnknownUser { user_id: id })?;
        let mut user = row.into_user()?;

        if let Some(display_name) = &update.display_name {
            let trimmed = display_name.trim();
            user.display_name = if trimmed.is_empty() {
                user.username.clone()
            } else {
                trimmed.to_owned()
            };
        }
        if let Some(is_admin) = update.is_admin {
            user.is_admin = is_admin;
        }
        if let Some(permissions) = update.permissions {
            user.can_acquire = permissions.acquire;
            user.can_upload = permissions.upload;
            user.can_download = permissions.download;
        }

        let now = crate::now_millis();
        sqlx::query!(
            "UPDATE users SET display_name = ?, is_admin = ?, can_acquire = ?, can_upload = ?, \
             can_download = ?, updated_at = ? WHERE id = ?",
            &user.display_name,
            user.is_admin,
            user.can_acquire,
            user.can_upload,
            user.can_download,
            now,
            id.as_bytes().as_slice(),
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        user.updated_at = now;
        Ok(user)
    }

    /// Replaces an account's password.
    ///
    /// The new password is hashed with Argon2id, so the previous hash is gone
    /// and cannot be recovered. Sessions and credentials that were issued
    /// earlier are not touched here; a caller that wants them revoked revokes
    /// them itself.
    ///
    /// # Parameters
    ///
    /// - `id`: the account whose password changes.
    /// - `password`: the new plaintext password.
    ///
    /// # Returns
    ///
    /// `Ok(())` once the hash is stored.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::EmptyField`] for an empty password,
    /// [`AuthError::UnknownUser`] when no account has that id,
    /// [`AuthError::Password`] or [`AuthError::Join`] when hashing fails, and
    /// [`AuthError::Database`] when the write fails.
    pub async fn set_password(&self, id: Uuid, password: &str) -> Result<(), AuthError> {
        if password.is_empty() {
            return Err(AuthError::EmptyField { field: "password" });
        }
        let password_hash = password::hash_password(password).await?;
        let now = crate::now_millis();
        let affected = sqlx::query!(
            "UPDATE users SET password_hash = ?, updated_at = ? WHERE id = ?",
            password_hash,
            now,
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(AuthError::UnknownUser { user_id: id });
        }
        Ok(())
    }

    /// Enables a disabled account.
    ///
    /// The `disabled_at` timestamp is cleared, so the account authenticates
    /// again; an account that is already enabled is left as it is apart from
    /// its `updated_at` timestamp.
    ///
    /// # Parameters
    ///
    /// - `id`: the account to enable.
    ///
    /// # Returns
    ///
    /// `Ok(())` once the account is enabled.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::UnknownUser`] when no account has that id, and
    /// [`AuthError::Database`] when the write fails.
    pub async fn enable(&self, id: Uuid) -> Result<(), AuthError> {
        let now = crate::now_millis();
        let affected = sqlx::query!(
            "UPDATE users SET disabled_at = NULL, updated_at = ? WHERE id = ?",
            now,
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(AuthError::UnknownUser { user_id: id });
        }
        Ok(())
    }
}

/// Reports whether a database error is a unique-constraint violation.
///
/// # Parameters
///
/// - `error`: the error a write returned.
///
/// # Returns
///
/// `true` when the error is a SQLite uniqueness violation.
fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.is_unique_violation())
}

#[cfg(test)]
mod tests {
    use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
    use hearthsound_db::{Database, MIGRATOR};

    use super::*;

    /// Opens a migrated temporary database.
    async fn database() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        (dir, database)
    }

    /// Resolves a settings snapshot, optionally overriding values.
    fn settings(env: &[(&str, &str)]) -> SettingsStore {
        let mut sources = SettingSources::default();
        for (key, value) in env {
            sources.env.insert((*key).to_owned(), (*value).to_owned());
        }
        SettingsStore::resolve(&SettingsRegistry::builtin(), &sources).unwrap()
    }

    #[tokio::test]
    async fn creates_a_user_with_default_permissions() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[]);

        let user = users
            .create(&settings, &NewUser::new("echo", "Echo", "hunter2", false))
            .await
            .unwrap();

        assert_eq!(user.username, "echo");
        assert_eq!(user.display_name, "Echo");
        assert!(!user.is_admin);
        assert!(user.can_acquire && user.can_upload && user.can_download);
        assert!(!user.is_disabled());
        assert!(user.password_hash().starts_with("$argon2id$"));
        assert!(!format!("{user:?}").contains("argon2"));
    }

    #[tokio::test]
    async fn create_honours_the_default_permission_setting() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[("HEARTHSOUND__USERS__DEFAULT_PERMISSIONS", "download")]);

        let user = users
            .create(&settings, &NewUser::new("echo", "", "hunter2", false))
            .await
            .unwrap();

        assert!(!user.can_acquire);
        assert!(!user.can_upload);
        assert!(user.can_download);
        assert_eq!(user.display_name, "echo");
    }

    #[tokio::test]
    async fn usernames_are_unique_case_insensitively() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[]);

        users
            .create(&settings, &NewUser::new("Echo", "Echo", "hunter2", false))
            .await
            .unwrap();

        let error = users
            .create(&settings, &NewUser::new("echo", "Echo", "hunter2", false))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::UsernameTaken { .. }));

        let found = users.find_by_username("ECHO").await.unwrap().unwrap();
        assert_eq!(found.username, "Echo");
    }

    #[tokio::test]
    async fn rejects_an_empty_username_or_password() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[]);

        let error = users
            .create(&settings, &NewUser::new("  ", "Echo", "hunter2", false))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::EmptyField { field: "username" }));

        let error = users
            .create(&settings, &NewUser::new("echo", "Echo", "", false))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::EmptyField { field: "password" }));
    }

    #[tokio::test]
    async fn lists_users_and_disables_them() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[]);

        let first = users
            .create(&settings, &NewUser::new("zoe", "Zoe", "hunter2", false))
            .await
            .unwrap();
        users
            .create(&settings, &NewUser::new("adam", "Adam", "hunter2", true))
            .await
            .unwrap();

        let listed = users.list().await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|user| user.username.as_str())
                .collect::<Vec<_>>(),
            ["adam", "zoe"]
        );

        assert!(users.disable(first.id).await.unwrap());
        assert!(!users.disable(first.id).await.unwrap());
        let disabled = users.find_by_id(first.id).await.unwrap().unwrap();
        assert!(disabled.is_disabled());

        assert!(!users.disable(new_uuid_v7()).await.unwrap());
    }

    #[test]
    fn permissions_round_trip_through_their_names() {
        for permission in Permission::ALL {
            assert_eq!(
                permission.as_str().parse::<Permission>().unwrap(),
                permission
            );
        }
        assert!("nope".parse::<Permission>().is_err());
    }

    #[tokio::test]
    async fn create_honours_explicit_permissions() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[]);

        let granted = UserPermissions {
            acquire: false,
            upload: false,
            download: true,
        };
        let user = users
            .create(
                &settings,
                &NewUser::new("echo", "Echo", "hunter2", false).with_permissions(granted),
            )
            .await
            .unwrap();

        assert!(!user.can_acquire);
        assert!(!user.can_upload);
        assert!(user.can_download);
        assert_eq!(user.permissions(), granted);
    }

    #[tokio::test]
    async fn update_changes_the_named_fields_and_leaves_the_rest() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[]);
        let user = users
            .create(&settings, &NewUser::new("echo", "Echo", "hunter2", false))
            .await
            .unwrap();

        let update = UserUpdate::default()
            .with_is_admin(true)
            .with_permissions(UserPermissions {
                acquire: false,
                upload: true,
                download: false,
            });
        let updated = users.update(user.id, &update).await.unwrap();

        assert!(updated.is_admin);
        assert!(!updated.can_acquire);
        assert!(updated.can_upload);
        assert!(!updated.can_download);
        assert_eq!(updated.display_name, "Echo", "the display name is kept");
        assert_eq!(updated.created_at, user.created_at);
        assert!(updated.updated_at >= user.updated_at);

        let stored = users.find_by_id(user.id).await.unwrap().unwrap();
        assert_eq!(stored.permissions(), updated.permissions());
        assert!(stored.is_admin);
        assert_eq!(stored.username, "echo");

        let renamed = users
            .update(
                user.id,
                &UserUpdate::default().with_display_name("  Echo  "),
            )
            .await
            .unwrap();
        assert_eq!(renamed.display_name, "Echo");
        assert!(renamed.can_upload, "the permissions are untouched");

        let blank = users
            .update(user.id, &UserUpdate::default().with_display_name("   "))
            .await
            .unwrap();
        assert_eq!(blank.display_name, "echo", "a blank name falls back");
    }

    #[tokio::test]
    async fn update_reports_an_unknown_account() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);

        let error = users
            .update(new_uuid_v7(), &UserUpdate::default().with_is_admin(true))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::UnknownUser { .. }));
    }

    #[tokio::test]
    async fn set_password_replaces_the_stored_hash() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[]);
        let user = users
            .create(&settings, &NewUser::new("echo", "Echo", "hunter2", false))
            .await
            .unwrap();
        let previous = user.password_hash().to_owned();

        users.set_password(user.id, "correct horse").await.unwrap();

        let stored = users.find_by_id(user.id).await.unwrap().unwrap();
        assert_ne!(stored.password_hash(), previous);
        assert!(stored.password_hash().starts_with("$argon2id$"));
        assert!(
            crate::verify_password("correct horse", stored.password_hash())
                .await
                .unwrap()
        );
        assert!(
            !crate::verify_password("hunter2", stored.password_hash())
                .await
                .unwrap()
        );
        assert!(stored.updated_at >= user.updated_at);

        let error = users.set_password(user.id, "").await.unwrap_err();
        assert!(matches!(error, AuthError::EmptyField { field: "password" }));

        let error = users
            .set_password(new_uuid_v7(), "hunter2")
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::UnknownUser { .. }));
    }

    #[tokio::test]
    async fn enable_clears_the_disabled_timestamp() {
        let (_dir, db) = database().await;
        let users = Users::new(&db);
        let settings = settings(&[]);
        let user = users
            .create(&settings, &NewUser::new("echo", "Echo", "hunter2", false))
            .await
            .unwrap();

        assert!(users.disable(user.id).await.unwrap());
        assert!(
            users
                .find_by_id(user.id)
                .await
                .unwrap()
                .unwrap()
                .is_disabled()
        );

        users.enable(user.id).await.unwrap();
        assert!(
            !users
                .find_by_id(user.id)
                .await
                .unwrap()
                .unwrap()
                .is_disabled()
        );

        users.enable(user.id).await.unwrap();

        let error = users.enable(new_uuid_v7()).await.unwrap_err();
        assert!(matches!(error, AuthError::UnknownUser { .. }));
    }
}
