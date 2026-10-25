//! Per-user setting overrides.
//!
//! A row in `user_settings` shadows the server-wide value of one setting for one
//! user. Only settings the registry marks user overridable may be written here,
//! and the value must satisfy the setting's kind - the same check the settings
//! store applies to its own layers - so a stored override always resolves. A
//! caller reads the override and falls back to the server snapshot itself, which
//! keeps the resolution rule (a user value wins, otherwise the server value) in
//! one place.

use hearthsound_config::{SettingDef, SettingKey, SettingType, SettingValue, SettingsRegistry};
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::AuthError;

/// One override stored for a user.
#[derive(Debug, Clone, PartialEq)]
pub struct UserSettingEntry {
    /// The dotted setting key.
    pub key: String,
    /// The user's value.
    pub value: SettingValue,
}

/// The `user_settings` table.
///
/// Cloning is cheap: every clone shares the same two pools and registry.
#[derive(Debug, Clone)]
pub struct UserSettings {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
    /// The registry that says which keys exist, which are overridable, and what
    /// shape their values have.
    registry: SettingsRegistry,
}

impl UserSettings {
    /// Builds the repository over a database and a settings registry.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    /// - `registry`: the settings the server knows, normally
    ///   [`SettingsRegistry::builtin`].
    ///
    /// # Returns
    ///
    /// The repository.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(db: &hearthsound_db::Database) {
    /// let settings = hearthsound_auth::UserSettings::new(
    ///     db,
    ///     hearthsound_config::SettingsRegistry::builtin(),
    /// );
    /// # let _ = settings;
    /// # }
    /// ```
    pub fn new(db: &Database, registry: SettingsRegistry) -> UserSettings {
        UserSettings {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            registry,
        }
    }

    /// Reads one override as an untyped value.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the override belongs to.
    /// - `key`: the dotted setting key.
    ///
    /// # Returns
    ///
    /// The stored value, or `None` when the user has no override for the key.
    /// A row whose value is no longer valid JSON is treated as absent and
    /// logged, so one bad row cannot break every request for that user.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    pub async fn get(&self, user_id: Uuid, key: &str) -> Result<Option<SettingValue>, AuthError> {
        let stored = sqlx::query_scalar!(
            "SELECT value FROM user_settings WHERE user_id = ? AND key = ?",
            user_id.as_bytes().as_slice(),
            key,
        )
        .fetch_optional(&self.readers)
        .await?;

        let Some(stored) = stored else {
            return Ok(None);
        };
        match SettingValue::from_json(key, &stored) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                tracing::warn!(
                    user = %user_id,
                    key,
                    error = %error,
                    "ignoring a stored user setting that is not valid JSON"
                );
                Ok(None)
            }
        }
    }

    /// Reads one override as the setting's declared type.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the override belongs to.
    /// - `key`: the typed key to read.
    ///
    /// # Returns
    ///
    /// The stored value as `T`, or `None` when the user has no override or the
    /// stored value is not representable as `T` (which the registry check
    /// prevents for values written through [`UserSettings::set`]).
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(settings: &hearthsound_auth::UserSettings, user_id: uuid::Uuid) -> Result<(), hearthsound_auth::AuthError> {
    /// use hearthsound_config::keys;
    ///
    /// let scope = settings
    ///     .get_typed(user_id, keys::SUBSONIC_DEFAULT_SCOPE)
    ///     .await?;
    /// assert!(scope.as_deref() == Some("server") || scope.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_typed<T: SettingType>(
        &self,
        user_id: Uuid,
        key: SettingKey<T>,
    ) -> Result<Option<T>, AuthError> {
        let stored = self.get(user_id, key.name()).await?;
        Ok(stored.as_ref().and_then(T::from_value))
    }

    /// Writes one override.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the override belongs to. The row is a foreign
    ///   key, so an unknown user is a database error.
    /// - `key`: the dotted setting key, which must be registered and user
    ///   overridable.
    /// - `value`: the user's value, which must satisfy the setting's kind.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::UnknownUserSetting`] when the registry does not know
    /// the key, [`AuthError::NotUserOverridable`] when the setting is
    /// server-only, [`AuthError::Setting`] when the value fails its kind, and
    /// [`AuthError::Database`] when the write fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(settings: &hearthsound_auth::UserSettings, user_id: uuid::Uuid) -> Result<(), hearthsound_auth::AuthError> {
    /// use hearthsound_config::{SettingValue, keys};
    ///
    /// settings
    ///     .set(
    ///         user_id,
    ///         keys::SUBSONIC_DEFAULT_SCOPE.name(),
    ///         SettingValue::String("server".to_owned()),
    ///     )
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set(
        &self,
        user_id: Uuid,
        key: &str,
        value: SettingValue,
    ) -> Result<(), AuthError> {
        self.definition(key)?
            .validate(&value)
            .map_err(AuthError::Setting)?;

        sqlx::query!(
            "INSERT INTO user_settings (user_id, key, value) VALUES (?, ?, ?)
             ON CONFLICT(user_id, key) DO UPDATE SET value = excluded.value",
            user_id.as_bytes().as_slice(),
            key,
            value.to_json(),
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Removes one override, falling back to the server value.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the override belongs to.
    /// - `key`: the dotted setting key.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the row is gone, whether or not it existed.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::UnknownUserSetting`] when the registry does not know
    /// the key, [`AuthError::NotUserOverridable`] when the setting is
    /// server-only, and [`AuthError::Database`] when the delete fails.
    pub async fn unset(&self, user_id: Uuid, key: &str) -> Result<(), AuthError> {
        self.definition(key)?;
        sqlx::query!(
            "DELETE FROM user_settings WHERE user_id = ? AND key = ?",
            user_id.as_bytes().as_slice(),
            key,
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Lists every valid override of a user.
    ///
    /// # Returns
    ///
    /// The stored overrides, ordered by key. Rows the registry no longer knows,
    /// rows for settings that stopped being user overridable, and rows holding
    /// invalid JSON are skipped, so a stale row cannot surface to a client.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Database`] when the query fails.
    pub async fn list(&self, user_id: Uuid) -> Result<Vec<UserSettingEntry>, AuthError> {
        let rows = sqlx::query!(
            "SELECT key, value FROM user_settings WHERE user_id = ? ORDER BY key",
            user_id.as_bytes().as_slice(),
        )
        .fetch_all(&self.readers)
        .await?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(def) = self.overridable(&row.key) else {
                continue;
            };
            match SettingValue::from_json(&row.key, &row.value) {
                Ok(value) if def.validate(&value).is_ok() => {
                    entries.push(UserSettingEntry {
                        key: row.key,
                        value,
                    });
                }
                Ok(_) => tracing::warn!(
                    user = %user_id,
                    key = %row.key,
                    "ignoring a stored user setting the registry rejects"
                ),
                Err(error) => tracing::warn!(
                    user = %user_id,
                    key = %row.key,
                    error = %error,
                    "ignoring a stored user setting that is not valid JSON"
                ),
            }
        }
        Ok(entries)
    }

    /// Looks up the definition of a key users may override.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key.
    ///
    /// # Returns
    ///
    /// The definition.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::UnknownUserSetting`] for an unregistered key and
    /// [`AuthError::NotUserOverridable`] for a server-only setting.
    fn definition(&self, key: &str) -> Result<&SettingDef, AuthError> {
        self.overridable(key)
            .ok_or_else(|| self.reject(key, self.registry.def(key)))
    }

    /// Returns the definition of a key that exists and is user overridable.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key.
    ///
    /// # Returns
    ///
    /// The definition, or `None` when the key is unknown or server-only.
    fn overridable(&self, key: &str) -> Option<&SettingDef> {
        self.registry.def(key).filter(|def| def.user_overridable)
    }

    /// Builds the error for a key users may not override.
    ///
    /// # Parameters
    ///
    /// - `key`: the rejected key.
    /// - `def`: the registered definition, or `None` when the key is unknown.
    ///
    /// # Returns
    ///
    /// [`AuthError::UnknownUserSetting`] for an unknown key and
    /// [`AuthError::NotUserOverridable`] otherwise.
    fn reject(&self, key: &str, def: Option<&SettingDef>) -> AuthError {
        match def {
            Some(_) => AuthError::NotUserOverridable {
                key: key.to_owned(),
            },
            None => AuthError::UnknownUserSetting {
                key: key.to_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use hearthsound_config::{SettingSources, SettingsRegistry, keys};
    use hearthsound_core::new_uuid_v7;
    use hearthsound_db::{Database, MIGRATOR};

    use super::*;
    use crate::{NewUser, Users};

    /// Opens a migrated temporary database with one account.
    async fn database() -> (tempfile::TempDir, Database, Uuid, UserSettings) {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        let registry = SettingsRegistry::builtin();
        let snapshot =
            hearthsound_config::SettingsStore::resolve(&registry, &SettingSources::default())
                .unwrap();
        let user = Users::new(&database)
            .create(&snapshot, &NewUser::new("echo", "Echo", "hunter22", false))
            .await
            .unwrap();
        let settings = UserSettings::new(&database, registry);
        (dir, database, user.id, settings)
    }

    #[tokio::test]
    async fn writes_reads_and_removes_an_override() {
        let (_dir, _db, user_id, settings) = database().await;

        assert_eq!(
            settings
                .get_typed(user_id, keys::SUBSONIC_DEFAULT_SCOPE)
                .await
                .unwrap(),
            None
        );

        settings
            .set(
                user_id,
                keys::SUBSONIC_DEFAULT_SCOPE.name(),
                SettingValue::String("server".to_owned()),
            )
            .await
            .unwrap();

        assert_eq!(
            settings
                .get_typed(user_id, keys::SUBSONIC_DEFAULT_SCOPE)
                .await
                .unwrap()
                .as_deref(),
            Some("server")
        );

        settings
            .set(
                user_id,
                keys::SUBSONIC_DEFAULT_SCOPE.name(),
                SettingValue::String("library".to_owned()),
            )
            .await
            .unwrap();
        let listed = settings.list(user_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].value, SettingValue::String("library".to_owned()));

        settings
            .unset(user_id, keys::SUBSONIC_DEFAULT_SCOPE.name())
            .await
            .unwrap();
        assert!(settings.list(user_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_unknown_and_server_only_keys() {
        let (_dir, _db, user_id, settings) = database().await;

        let error = settings
            .set(user_id, "nope.nope", SettingValue::Bool(true))
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::UnknownUserSetting { .. }));

        let error = settings
            .set(
                user_id,
                keys::STREAM_PENDING_MODE.name(),
                SettingValue::String("hold".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::NotUserOverridable { .. }));

        let error = settings
            .unset(user_id, keys::STREAM_PENDING_MODE.name())
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::NotUserOverridable { .. }));
    }

    #[tokio::test]
    async fn rejects_a_value_of_the_wrong_kind() {
        let (_dir, _db, user_id, settings) = database().await;

        let error = settings
            .set(
                user_id,
                keys::SUBSONIC_DEFAULT_SCOPE.name(),
                SettingValue::String("everywhere".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Setting(_)));

        let error = settings
            .set(
                user_id,
                keys::PRIVACY_NOW_PLAYING_VISIBLE.name(),
                SettingValue::Integer(3),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Setting(_)));
    }

    #[tokio::test]
    async fn overrides_are_per_user() {
        let (_dir, db, user_id, settings) = database().await;
        let registry = SettingsRegistry::builtin();
        let snapshot =
            hearthsound_config::SettingsStore::resolve(&registry, &SettingSources::default())
                .unwrap();
        let other = Users::new(&db)
            .create(
                &snapshot,
                &NewUser::new("other", "Other", "hunter22", false),
            )
            .await
            .unwrap();

        settings
            .set(
                user_id,
                keys::SUBSONIC_DEFAULT_SCOPE.name(),
                SettingValue::String("server".to_owned()),
            )
            .await
            .unwrap();

        assert_eq!(settings.list(other.id).await.unwrap().len(), 0);
        assert_eq!(settings.list(user_id).await.unwrap().len(), 1);
        assert!(
            settings
                .get(other.id, keys::SUBSONIC_DEFAULT_SCOPE.name())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unknown_user_is_a_foreign_key_error() {
        let (_dir, _db, _user_id, settings) = database().await;

        let error = settings
            .set(
                new_uuid_v7(),
                keys::SUBSONIC_DEFAULT_SCOPE.name(),
                SettingValue::String("server".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Database(_)));
    }
}
