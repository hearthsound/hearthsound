//! The runtime settings service.
//!
//! Runtime settings live in the `settings` table of the database, where the API
//! and the admin UI edit them, and are resolved with the precedence
//! environment, config file, database, built-in default. The config file is
//! special: a key in its `[settings]` table pins the value read-only, so a write
//! through [`Settings::set`] is rejected with [`SettingsError::Pinned`], and a
//! key that an environment variable overrides is rejected with
//! [`SettingsError::EnvironmentOverride`]. Both are conflicts the HTTP layer
//! maps to 409.
//!
//! [`Settings`] keeps the resolved [`SettingsStore`] behind an `Arc` in a tokio
//! `watch` channel: readers clone the current snapshot cheaply with
//! [`Settings::current`], and subscribers that need to react to an edit await
//! [`Settings::subscribe`]. Every write re-reads the whole database layer and
//! republishes the snapshot, so an edit made by another process is picked up by
//! [`Settings::reload`] without a restart.

use std::collections::BTreeMap;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    ConfigError, SettingSource, SettingSources, SettingValue, SettingsRegistry, SettingsStore,
    env_name_for, validate,
};

/// A read-only view of one setting, as reported by [`Settings::list`].
#[derive(Debug, Clone, PartialEq)]
pub struct SettingEntry {
    /// The dotted setting key.
    pub key: &'static str,
    /// The effective value, after precedence.
    pub value: SettingValue,
    /// The built-in default.
    pub default: SettingValue,
    /// The layer that supplied the effective value.
    pub source: SettingSource,
    /// A one-line human readable description.
    pub description: &'static str,
    /// Whether the config file pins the key, which makes it read-only.
    pub pinned: bool,
    /// Whether a user may override the setting for themselves.
    pub user_overridable: bool,
}

/// An error from reading or writing a runtime setting.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The key is not in the registry.
    #[error("setting `{key}` is not registered")]
    UnknownKey {
        /// The unrecognized key.
        key: String,
    },
    /// The config file pins the key, so it cannot be edited through the API.
    #[error("setting `{key}` is pinned by the configuration file")]
    Pinned {
        /// The pinned key.
        key: String,
    },
    /// An environment variable overrides the key, so a stored value would never
    /// take effect.
    #[error("setting `{key}` is overridden by the environment")]
    EnvironmentOverride {
        /// The overridden key.
        key: String,
    },
    /// A value did not satisfy the setting's kind.
    #[error(transparent)]
    Invalid(#[from] ConfigError),
    /// The database rejected the read or the write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// The shared runtime settings service.
///
/// Cloning is cheap: every clone shares one registry, one database pool, and one
/// watch channel.
#[derive(Debug, Clone)]
pub struct Settings(Arc<Inner>);

/// The state shared by every clone of [`Settings`].
#[derive(Debug)]
struct Inner {
    /// The settings the service knows.
    registry: SettingsRegistry,
    /// The environment layer, keyed by full variable name.
    env: BTreeMap<String, String>,
    /// The config file layer, keyed by dotted setting key. Its keys are pinned.
    file: BTreeMap<String, toml::Value>,
    /// The pool the `settings` table is read from and written to.
    pool: SqlitePool,
    /// The channel carrying the current snapshot.
    sender: watch::Sender<Arc<SettingsStore>>,
}

impl Settings {
    /// Loads the stored layer and builds the service.
    ///
    /// # Parameters
    ///
    /// - `pool`: a pool connected to the migrated database.
    /// - `registry`: the settings the service knows, normally
    ///   [`SettingsRegistry::builtin`].
    /// - `sources`: the environment and file layers; its database layer is
    ///   replaced by the contents of the `settings` table.
    ///
    /// # Returns
    ///
    /// The service, whose current snapshot is already resolved.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Database`] when the table cannot be read, and
    /// [`SettingsError::Invalid`] when a stored value or a file value is
    /// malformed or out of range.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use sqlx::SqlitePool;
    /// use hearthsound_config::{SettingSources, SettingsRegistry, settings::Settings};
    ///
    /// # async fn example(pool: SqlitePool) -> Result<(), Box<dyn std::error::Error>> {
    /// let settings = Settings::load(
    ///     pool,
    ///     SettingsRegistry::builtin(),
    ///     SettingSources::default(),
    /// )
    /// .await?;
    /// println!("{} settings", settings.list().len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn load(
        pool: SqlitePool,
        registry: SettingsRegistry,
        mut sources: SettingSources,
    ) -> Result<Settings, SettingsError> {
        sources.database = repository(&pool).load(&registry).await?;
        let store = SettingsStore::resolve(&registry, &sources)?;
        let (sender, _receiver) = watch::channel(Arc::new(store));
        Ok(Settings(Arc::new(Inner {
            registry,
            env: sources.env,
            file: sources.file,
            pool,
            sender,
        })))
    }

    /// Returns the current snapshot.
    ///
    /// The snapshot is immutable; a caller that wants a later one subscribes
    /// with [`Settings::subscribe`] or asks for this method again.
    ///
    /// # Returns
    ///
    /// The resolved settings in effect right now.
    pub fn current(&self) -> Arc<SettingsStore> {
        self.0.sender.borrow().clone()
    }

    /// Subscribes to snapshot changes.
    ///
    /// # Returns
    ///
    /// A receiver that marks itself changed after a write or a reload whose
    /// resolved snapshot differs from the previous one. A write that leaves the
    /// snapshot unchanged (the same value from the same layer) notifies nobody.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use sqlx::SqlitePool;
    /// # use hearthsound_config::{SettingValue, SettingSources, SettingsRegistry, settings::Settings, keys};
    /// # async fn example(settings: Settings) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut changes = settings.subscribe();
    /// settings.set(keys::STREAM_PENDING_MODE.name(), SettingValue::String("hold".to_owned()), None).await?;
    /// changes.changed().await?;
    /// assert_eq!(changes.borrow().get(keys::STREAM_PENDING_MODE)?, "hold");
    /// # Ok(())
    /// # }
    /// ```
    pub fn subscribe(&self) -> watch::Receiver<Arc<SettingsStore>> {
        self.0.sender.subscribe()
    }

    /// Returns the registry of known settings.
    pub fn registry(&self) -> &SettingsRegistry {
        &self.0.registry
    }

    /// Stores a value in the database and republishes the snapshot.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key, which must be registered.
    /// - `value`: the new value, which must satisfy the setting's kind.
    /// - `updated_by`: the user making the change, stored for auditing. The user
    ///   must exist, because the column is a foreign key.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::UnknownKey`] for an unregistered key,
    /// [`SettingsError::Pinned`] when the config file pins the key,
    /// [`SettingsError::EnvironmentOverride`] when an environment variable
    /// overrides it, [`SettingsError::Invalid`] when the value does not satisfy
    /// the setting's kind, and [`SettingsError::Database`] when the write fails.
    pub async fn set(
        &self,
        key: &str,
        value: SettingValue,
        updated_by: Option<Uuid>,
    ) -> Result<(), SettingsError> {
        let def = self
            .0
            .registry
            .def(key)
            .ok_or_else(|| SettingsError::UnknownKey {
                key: key.to_owned(),
            })?;
        self.check_writable(key)?;
        validate(def, &value, SettingSource::Database)?;
        repository(&self.0.pool)
            .upsert(key, &value.to_json(), updated_by)
            .await?;
        self.refresh().await
    }

    /// Removes a stored value, falling back to the layers below it.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key, which must be registered.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the row is gone, whether or not it existed.
    ///
    /// # Errors
    ///
    /// Same as [`Settings::set`], minus the value validation.
    pub async fn unset(&self, key: &str) -> Result<(), SettingsError> {
        if self.0.registry.def(key).is_none() {
            return Err(SettingsError::UnknownKey {
                key: key.to_owned(),
            });
        }
        self.check_writable(key)?;
        repository(&self.0.pool).delete(key).await?;
        self.refresh().await
    }

    /// Re-reads the stored layer and republishes the snapshot.
    ///
    /// Call this when another process may have changed the `settings` table.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Database`] when the table cannot be read, and
    /// [`SettingsError::Invalid`] when a stored value no longer satisfies its
    /// setting's kind.
    pub async fn reload(&self) -> Result<(), SettingsError> {
        self.refresh().await
    }

    /// Lists every registered setting with its effective value.
    ///
    /// # Returns
    ///
    /// One entry per registered setting, in registry order: the key, the
    /// effective value, the built-in default, the layer that won, the
    /// description, whether the config file pins the key, and whether a user may
    /// override it.
    pub fn list(&self) -> Vec<SettingEntry> {
        let store = self.current();
        self.0
            .registry
            .iter()
            .map(|def| SettingEntry {
                key: def.key,
                value: store
                    .value(def.key)
                    .cloned()
                    .unwrap_or_else(|| def.default.clone()),
                default: def.default.clone(),
                source: store.source(def.key).unwrap_or(SettingSource::Default),
                description: def.description,
                pinned: self.0.file.contains_key(def.key),
                user_overridable: def.user_overridable,
            })
            .collect()
    }

    /// Rejects a key that cannot be edited through the database.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Pinned`] when the config file holds the key and
    /// [`SettingsError::EnvironmentOverride`] when an environment variable
    /// overrides it.
    fn check_writable(&self, key: &str) -> Result<(), SettingsError> {
        if self.0.file.contains_key(key) {
            return Err(SettingsError::Pinned {
                key: key.to_owned(),
            });
        }
        if self.0.env.contains_key(&env_name_for(key)) {
            return Err(SettingsError::EnvironmentOverride {
                key: key.to_owned(),
            });
        }
        Ok(())
    }

    /// Re-reads the stored layer and notifies subscribers when anything moved.
    ///
    /// # Errors
    ///
    /// Returns what [`Settings::reload`] documents.
    async fn refresh(&self) -> Result<(), SettingsError> {
        let mut sources = SettingSources::new(self.0.env.clone(), self.0.file.clone());
        sources.database = repository(&self.0.pool).load(&self.0.registry).await?;
        let store = SettingsStore::resolve(&self.0.registry, &sources)?;
        if **self.0.sender.borrow() != store {
            self.0.sender.send_replace(Arc::new(store));
        }
        Ok(())
    }
}

/// Builds a repository over a pool.
///
/// # Parameters
///
/// - `pool`: the pool holding the `settings` table.
///
/// # Returns
///
/// The repository.
fn repository(pool: &SqlitePool) -> SettingsRepository {
    SettingsRepository { pool: pool.clone() }
}

/// The `settings` table of the database.
///
/// Values are stored as JSON text so booleans, numbers, strings, and string
/// lists round-trip without a type column.
#[derive(Debug)]
struct SettingsRepository {
    /// The pool the table is read from and written to.
    pool: SqlitePool,
}

impl SettingsRepository {
    /// Reads every stored setting that is registered.
    ///
    /// A row whose key the registry does not know is skipped: keys can be
    /// removed between releases while their rows stay behind.
    ///
    /// # Parameters
    ///
    /// - `registry`: the settings to accept.
    ///
    /// # Returns
    ///
    /// The stored values, keyed by dotted setting key.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Database`] when the table cannot be read and
    /// [`SettingsError::Invalid`] when a stored value is not valid JSON for its
    /// setting.
    async fn load(
        &self,
        registry: &SettingsRegistry,
    ) -> Result<BTreeMap<String, SettingValue>, SettingsError> {
        let rows = sqlx::query!("SELECT key, value FROM settings")
            .fetch_all(&self.pool)
            .await?;
        let mut values = BTreeMap::new();
        for row in rows {
            if registry.def(&row.key).is_none() {
                tracing::debug!(
                    key = %row.key,
                    "ignoring a stored setting that is not registered"
                );
                continue;
            }
            values.insert(
                row.key.clone(),
                SettingValue::from_json(&row.key, &row.value)?,
            );
        }
        Ok(values)
    }

    /// Writes one setting, replacing any stored value.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key.
    /// - `value`: the JSON text to store.
    /// - `updated_by`: the user making the change, or `None` for a change the
    ///   server made itself.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Database`] when the write fails, including when
    /// `updated_by` names a user that does not exist.
    async fn upsert(
        &self,
        key: &str,
        value: &str,
        updated_by: Option<Uuid>,
    ) -> Result<(), SettingsError> {
        let updated_at = now_millis();
        let updated_by = updated_by.map(|user| user.as_bytes().to_vec());
        sqlx::query!(
            "INSERT INTO settings (key, value, updated_at, updated_by)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET
               value = excluded.value,
               updated_at = excluded.updated_at,
               updated_by = excluded.updated_by",
            key,
            value,
            updated_at,
            updated_by.as_deref(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes one setting.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Database`] when the delete fails.
    async fn delete(&self, key: &str) -> Result<(), SettingsError> {
        sqlx::query!("DELETE FROM settings WHERE key = ?", key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Returns the current time in unix milliseconds.
fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}

#[cfg(test)]
mod tests {
    use hearthsound_db::{Database, MIGRATOR};

    use super::*;
    use crate::keys;

    /// Creates a migrated temporary database and the pool for its settings.
    async fn database() -> (tempfile::TempDir, Database, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hearthsound.db");
        let database = Database::open(&path).await.unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        let pool = database.writer().clone();
        (dir, database, pool)
    }

    /// Loads the service with no environment or file layer.
    async fn settings(pool: SqlitePool) -> Settings {
        Settings::load(pool, SettingsRegistry::builtin(), SettingSources::default())
            .await
            .unwrap()
    }

    /// Builds a source set with one file-pinned key and one environment
    /// override.
    fn layered_sources() -> SettingSources {
        let mut sources = SettingSources::default();
        sources.file.insert(
            "stream.pending.mode".to_owned(),
            toml::Value::String("fail".to_owned()),
        );
        sources.env.insert(
            "HEARTHSOUND__SEARCH__CATALOG__MAX_RESULTS".to_owned(),
            "7".to_owned(),
        );
        sources
    }

    #[tokio::test]
    async fn loads_stored_values_and_reports_their_source() {
        let (_dir, _database, pool) = database().await;
        let settings = settings(pool).await;

        assert_eq!(
            settings.current().get(keys::STREAM_PENDING_MODE).unwrap(),
            "hold"
        );
        assert_eq!(
            settings.current().source(keys::STREAM_PENDING_MODE.name()),
            Some(SettingSource::Default)
        );

        settings
            .set(
                keys::STREAM_PENDING_MODE.name(),
                SettingValue::String("fail".to_owned()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            settings.current().get(keys::STREAM_PENDING_MODE).unwrap(),
            "fail"
        );
        assert_eq!(
            settings.current().source(keys::STREAM_PENDING_MODE.name()),
            Some(SettingSource::Database)
        );

        settings
            .unset(keys::STREAM_PENDING_MODE.name())
            .await
            .unwrap();
        assert_eq!(
            settings.current().get(keys::STREAM_PENDING_MODE).unwrap(),
            "hold"
        );
        assert_eq!(
            settings.current().source(keys::STREAM_PENDING_MODE.name()),
            Some(SettingSource::Default)
        );
    }

    #[tokio::test]
    async fn precedence_is_environment_then_file_then_database() {
        let (_dir, _database, pool) = database().await;
        // Seed the stored layer directly: the environment and file layers would
        // reject a write for the two keys they hold.
        for (key, value) in [
            (keys::SEARCH_CATALOG_MAX_RESULTS.name(), "3"),
            (keys::STREAM_PENDING_MODE.name(), "\"hold\""),
            ("privacy.server_browse", "\"all\""),
        ] {
            sqlx::query("INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)")
                .bind(key)
                .bind(value)
                .bind(now_millis())
                .execute(&pool)
                .await
                .unwrap();
        }

        let settings = Settings::load(pool, SettingsRegistry::builtin(), layered_sources())
            .await
            .unwrap();

        let source = |key: &str| settings.current().source(key);
        // The environment wins over the file, the stored value, and the default.
        assert_eq!(
            settings
                .current()
                .get(keys::SEARCH_CATALOG_MAX_RESULTS)
                .unwrap(),
            7
        );
        assert_eq!(
            source(keys::SEARCH_CATALOG_MAX_RESULTS.name()),
            Some(SettingSource::Environment)
        );
        // The file wins over the stored value.
        assert_eq!(
            settings.current().get(keys::STREAM_PENDING_MODE).unwrap(),
            "fail"
        );
        assert_eq!(
            source(keys::STREAM_PENDING_MODE.name()),
            Some(SettingSource::File)
        );
        // Nothing above the database holds this key.
        assert_eq!(
            settings.current().get(keys::PRIVACY_SERVER_BROWSE).unwrap(),
            "all"
        );
        assert_eq!(
            source(keys::PRIVACY_SERVER_BROWSE.name()),
            Some(SettingSource::Database)
        );
        // A key no layer holds keeps its built-in default.
        assert_eq!(
            source(keys::UPLOADS_ENABLED.name()),
            Some(SettingSource::Default)
        );
    }

    #[tokio::test]
    async fn pinned_and_environment_keys_are_rejected() {
        let (_dir, _database, pool) = database().await;
        let settings = Settings::load(pool, SettingsRegistry::builtin(), layered_sources())
            .await
            .unwrap();

        let pinned = settings
            .set(
                keys::STREAM_PENDING_MODE.name(),
                SettingValue::String("hold".to_owned()),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            pinned,
            SettingsError::Pinned { ref key } if key == "stream.pending.mode"
        ));
        let unset = settings
            .unset(keys::STREAM_PENDING_MODE.name())
            .await
            .unwrap_err();
        assert!(matches!(unset, SettingsError::Pinned { .. }));

        let overridden = settings
            .set(
                keys::SEARCH_CATALOG_MAX_RESULTS.name(),
                SettingValue::Integer(1),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            overridden,
            SettingsError::EnvironmentOverride { ref key } if key == "search.catalog.max_results"
        ));

        let unknown = settings
            .set("stream.nope", SettingValue::Bool(true), None)
            .await
            .unwrap_err();
        assert!(matches!(
            unknown,
            SettingsError::UnknownKey { ref key } if key == "stream.nope"
        ));
    }

    #[tokio::test]
    async fn invalid_values_are_rejected_before_the_write() {
        let (_dir, _database, pool) = database().await;
        let settings = settings(pool).await;

        let error = settings
            .set(
                keys::STREAM_PENDING_MAX_WAIT_SECS.name(),
                SettingValue::Integer(0),
                None,
            )
            .await
            .unwrap_err();
        match error {
            SettingsError::Invalid(ConfigError::InvalidValue { key, reason, .. }) => {
                assert_eq!(key, "stream.pending.max_wait_secs");
                assert_eq!(reason, "must be between 1 and 3600");
            }
            other => panic!("unexpected error: {other}"),
        }

        let error = settings
            .set(
                keys::STREAM_PENDING_MODE.name(),
                SettingValue::Bool(true),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, SettingsError::Invalid(_)));

        assert_eq!(
            settings
                .current()
                .get(keys::STREAM_PENDING_MAX_WAIT_SECS)
                .unwrap(),
            180
        );
    }

    #[tokio::test]
    async fn subscribers_are_notified_when_the_snapshot_moves() {
        let (_dir, _database, pool) = database().await;
        let settings = settings(pool).await;
        let mut changes = settings.subscribe();

        settings
            .set(
                keys::STREAM_PENDING_MODE.name(),
                SettingValue::String("fail".to_owned()),
                None,
            )
            .await
            .unwrap();
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow().get(keys::STREAM_PENDING_MODE).unwrap(),
            "fail"
        );

        // Writing the same value from the same layer leaves the snapshot alone.
        settings
            .set(
                keys::STREAM_PENDING_MODE.name(),
                SettingValue::String("fail".to_owned()),
                None,
            )
            .await
            .unwrap();
        assert!(!changes.has_changed().unwrap());

        settings
            .unset(keys::STREAM_PENDING_MODE.name())
            .await
            .unwrap();
        changes.changed().await.unwrap();
        assert_eq!(
            changes.borrow().get(keys::STREAM_PENDING_MODE).unwrap(),
            "hold"
        );
    }

    #[tokio::test]
    async fn reload_picks_up_a_write_from_another_connection() {
        let (_dir, _database, pool) = database().await;
        let settings = settings(pool.clone()).await;
        let mut changes = settings.subscribe();

        let other = Database::open(&database_path(&pool).await).await.unwrap();
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind("subsonic.catalog_title_suffix")
        .bind("\" (Deezer)\"")
        .bind(now_millis())
        .execute(other.writer())
        .await
        .unwrap();

        assert_eq!(
            settings
                .current()
                .get(keys::SUBSONIC_CATALOG_TITLE_SUFFIX)
                .unwrap(),
            ""
        );
        assert!(!changes.has_changed().unwrap());

        settings.reload().await.unwrap();
        assert_eq!(
            settings
                .current()
                .get(keys::SUBSONIC_CATALOG_TITLE_SUFFIX)
                .unwrap(),
            " (Deezer)"
        );
        changes.changed().await.unwrap();
        assert_eq!(
            changes
                .borrow()
                .get(keys::SUBSONIC_CATALOG_TITLE_SUFFIX)
                .unwrap(),
            " (Deezer)"
        );
        other.close().await;
    }

    /// Returns the file a pool is connected to, for a second connection.
    async fn database_path(pool: &SqlitePool) -> std::path::PathBuf {
        let file: String =
            sqlx::query_scalar("SELECT file FROM pragma_database_list WHERE name = 'main'")
                .fetch_one(pool)
                .await
                .unwrap();
        std::path::PathBuf::from(file)
    }

    #[tokio::test]
    async fn stored_rows_record_the_editing_user() {
        let (_dir, _database, pool) = database().await;
        let settings = settings(pool.clone()).await;
        let user = Uuid::now_v7();
        let now = now_millis();
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(user.as_bytes().to_vec())
        .bind("ada")
        .bind("Ada")
        .bind("hash")
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        settings
            .set(
                keys::UPLOADS_ENABLED.name(),
                SettingValue::Bool(false),
                Some(user),
            )
            .await
            .unwrap();

        let stored: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT updated_by FROM settings WHERE key = ?")
                .bind(keys::UPLOADS_ENABLED.name())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), Some(user.as_bytes().as_slice()));
        let value: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(keys::UPLOADS_ENABLED.name())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(value, "false");
    }

    #[tokio::test]
    async fn rows_for_unregistered_keys_are_ignored() {
        let (_dir, _database, pool) = database().await;
        sqlx::query("INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)")
            .bind("acquisition.fast_start.enabled")
            .bind("true")
            .bind(now_millis())
            .execute(&pool)
            .await
            .unwrap();

        let settings = settings(pool).await;
        assert_eq!(
            settings.current().get(keys::STREAM_PENDING_MODE).unwrap(),
            "hold"
        );
    }

    #[tokio::test]
    async fn a_corrupt_stored_value_is_reported() {
        let (_dir, _database, pool) = database().await;
        sqlx::query("INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)")
            .bind(keys::STREAM_PENDING_MODE.name())
            .bind("\"nonsense\"")
            .bind(now_millis())
            .execute(&pool)
            .await
            .unwrap();

        let error = Settings::load(pool, SettingsRegistry::builtin(), SettingSources::default())
            .await
            .unwrap_err();
        match error {
            SettingsError::Invalid(ConfigError::InvalidValue {
                key,
                origin,
                reason,
                ..
            }) => {
                assert_eq!(key, "stream.pending.mode");
                assert_eq!(origin, SettingSource::Database);
                assert_eq!(reason, "must be one of: hold, fail");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn list_reports_every_setting_with_its_layer() {
        let (_dir, _database, pool) = database().await;
        let settings = Settings::load(pool, SettingsRegistry::builtin(), layered_sources())
            .await
            .unwrap();

        let entries = settings.list();
        assert_eq!(entries.len(), settings.registry().iter().count());

        let pinned = entries
            .iter()
            .find(|entry| entry.key == keys::STREAM_PENDING_MODE.name())
            .unwrap();
        assert_eq!(pinned.value, SettingValue::String("fail".to_owned()));
        assert_eq!(pinned.default, SettingValue::String("hold".to_owned()));
        assert_eq!(pinned.source, SettingSource::File);
        assert!(pinned.pinned);
        assert!(!pinned.user_overridable);
        assert!(!pinned.description.is_empty());

        let overridable = entries
            .iter()
            .find(|entry| entry.key == keys::SUBSONIC_DEFAULT_SCOPE.name())
            .unwrap();
        assert!(overridable.user_overridable);
        assert!(!overridable.pinned);
        assert_eq!(overridable.source, SettingSource::Default);
    }
}
