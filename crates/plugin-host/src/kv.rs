//! The `host-kv` import: a persistent key-value store per plugin instance.
//!
//! Plugins are recycled between calls, so anything a plugin must remember goes
//! through this store, which persists in the `plugin_kv` table keyed by the
//! instance. The host owns the limits:
//!
//! - a value larger than [`MAX_VALUE_BYTES`] (256 KiB) is refused;
//! - the total size of an instance's rows may not exceed its approved quota
//!   (1 MiB by default, at most [`MAX_QUOTA_BYTES`] if the manifest asked for
//!   more), and a write that would exceed it **fails** rather than evicting a
//!   different key;
//! - an entry written with a TTL is not returned by a read once it expires, and
//!   [`sweep_expired`] reclaims the rows the server schedules it for.

use hearthsound_core::new_uuid_v7;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::manifest::KvPermission;

/// The largest value one key may hold.
pub const MAX_VALUE_BYTES: u64 = 256 * 1024;

/// The quota an instance gets when its manifest asks for none.
pub const DEFAULT_QUOTA_BYTES: u64 = 1024 * 1024;

/// The largest quota a manifest may request.
pub const MAX_QUOTA_BYTES: u64 = 64 * 1024 * 1024;

/// Why a key-value operation failed.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// The value is larger than the per-value cap.
    #[error("the value is {size} bytes, above the {limit} byte per-value cap")]
    ValueTooLarge {
        /// The rejected value's size.
        size: usize,
        /// The per-value cap.
        limit: u64,
    },
    /// The write would push the instance over its quota.
    #[error("storing {requested} bytes would exceed the instance's {quota} byte quota")]
    QuotaExceeded {
        /// The size the instance would hold after the write.
        requested: u64,
        /// The instance's quota.
        quota: u64,
    },
    /// The store could not be read or written.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// The key-value store of one plugin instance.
///
/// The store is cheap to clone-by-reference and holds no state of its own: it
/// carries the pool, the instance id and the quota so every call checks the same
/// limits without threading them through.
pub struct KvStore {
    pool: SqlitePool,
    instance_id: Uuid,
    quota_bytes: u64,
}

impl KvStore {
    /// Builds the store of one instance.
    ///
    /// # Parameters
    ///
    /// - `pool`: the database writer pool. Every write goes through it.
    /// - `instance_id`: the `provider_instances.id` the rows belong to.
    /// - `quota_bytes`: the instance's approved total quota. Values above
    ///   [`MAX_QUOTA_BYTES`] are clamped, and zero falls back to
    ///   [`DEFAULT_QUOTA_BYTES`].
    pub fn new(pool: SqlitePool, instance_id: Uuid, quota_bytes: u64) -> KvStore {
        let quota_bytes = if quota_bytes == 0 {
            DEFAULT_QUOTA_BYTES
        } else {
            quota_bytes.min(MAX_QUOTA_BYTES)
        };
        KvStore {
            pool,
            instance_id,
            quota_bytes,
        }
    }

    /// Builds a store from an approved KV permission and a database pool.
    ///
    /// # Parameters
    ///
    /// - `pool`: the database writer pool.
    /// - `instance_id`: the instance the rows belong to.
    /// - `permission`: the instance's approved `kv` permission, or `None` when
    ///   it was not approved for storage.
    ///
    /// # Returns
    ///
    /// A store whose quota is the permission's `maxBytes` clamped to
    /// [`MAX_QUOTA_BYTES`].
    pub fn from_permission(
        pool: SqlitePool,
        instance_id: Uuid,
        permission: Option<&KvPermission>,
    ) -> KvStore {
        let quota = permission
            .and_then(|kv| kv.max_bytes)
            .unwrap_or(DEFAULT_QUOTA_BYTES);
        KvStore::new(pool, instance_id, quota)
    }

    /// Generates the id of a new instance.
    ///
    /// A caller that is not loading an instance from the database uses this so
    /// the store still has a stable identity for its rows.
    pub fn new_instance_id() -> Uuid {
        new_uuid_v7()
    }

    /// Returns the instance id the store's rows belong to.
    pub fn instance_id(&self) -> Uuid {
        self.instance_id
    }

    /// Returns the instance's quota in bytes.
    pub fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }

    /// Reads one key.
    ///
    /// # Parameters
    ///
    /// - `key`: the key to read.
    ///
    /// # Returns
    ///
    /// The stored bytes, or `None` when the key is absent or has expired.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Database`] when the row cannot be read.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let row = sqlx::query!(
            "SELECT value, expires_at FROM plugin_kv WHERE instance_id = ? AND key = ?",
            self.instance_id.as_bytes().as_slice(),
            key,
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        if row
            .expires_at
            .is_some_and(|expires_at| expires_at <= now_millis())
        {
            return Ok(None);
        }
        Ok(Some(row.value))
    }

    /// Writes one key, replacing any previous value.
    ///
    /// The per-value cap and the total quota are checked inside the same
    /// transaction as the write, so two concurrent writes cannot both pass the
    /// quota check.
    ///
    /// # Parameters
    ///
    /// - `key`: the key to write.
    /// - `value`: the bytes to store.
    /// - `ttl_ms`: how long the entry stays readable, in milliseconds; `None`
    ///   keeps it until it is overwritten or deleted.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::ValueTooLarge`] when the value exceeds the per-value
    /// cap, [`KvError::QuotaExceeded`] when the write would exceed the
    /// instance's quota, and [`KvError::Database`] when the row cannot be
    /// written.
    pub async fn set(&self, key: &str, value: &[u8], ttl_ms: Option<u64>) -> Result<(), KvError> {
        if value.len() as u64 > MAX_VALUE_BYTES {
            return Err(KvError::ValueTooLarge {
                size: value.len(),
                limit: MAX_VALUE_BYTES,
            });
        }

        let expires_at = ttl_ms.map(|ttl| now_millis() + ttl as i64);
        let mut transaction = self.pool.begin().await?;

        let used: i64 = sqlx::query_scalar!(
            "SELECT COALESCE(SUM(LENGTH(value)), 0) FROM plugin_kv
             WHERE instance_id = ? AND key != ?",
            self.instance_id.as_bytes().as_slice(),
            key,
        )
        .fetch_one(&mut *transaction)
        .await?;

        let total = used.max(0) as u64 + value.len() as u64;
        if total > self.quota_bytes {
            return Err(KvError::QuotaExceeded {
                requested: total,
                quota: self.quota_bytes,
            });
        }

        sqlx::query!(
            "INSERT INTO plugin_kv (instance_id, key, value, expires_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(instance_id, key) DO UPDATE SET
               value = excluded.value,
               expires_at = excluded.expires_at",
            self.instance_id.as_bytes().as_slice(),
            key,
            value,
            expires_at,
        )
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;
        Ok(())
    }

    /// Removes one key.
    ///
    /// Deleting a key that is absent is not an error.
    ///
    /// # Parameters
    ///
    /// - `key`: the key to remove.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Database`] when the row cannot be deleted.
    pub async fn delete(&self, key: &str) -> Result<(), KvError> {
        sqlx::query!(
            "DELETE FROM plugin_kv WHERE instance_id = ? AND key = ?",
            self.instance_id.as_bytes().as_slice(),
            key,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Deletes every expired key-value row.
///
/// Expiry is honoured on read, so this is housekeeping rather than a
/// correctness requirement: the server schedules it to keep the table from
/// growing without bound. It is idempotent and safe to call concurrently.
///
/// # Parameters
///
/// - `pool`: the database writer pool.
///
/// # Returns
///
/// The number of rows removed.
///
/// # Errors
///
/// Returns [`KvError::Database`] when the delete fails.
pub async fn sweep_expired(pool: &SqlitePool) -> Result<u64, KvError> {
    let result = sqlx::query!(
        "DELETE FROM plugin_kv WHERE expires_at IS NOT NULL AND expires_at <= ?",
        now_millis(),
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Returns the current time in unix milliseconds UTC.
fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}

impl KvError {
    /// Returns the reason a plugin is told about.
    pub fn reason(&self) -> String {
        match self {
            KvError::Database(error) => {
                tracing::error!(
                    target: "hearthsound::plugin",
                    error = %error,
                    "plugin storage failed"
                );
                "the plugin's storage is unavailable".to_owned()
            }
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_db::{Database, MIGRATOR};

    /// Creates a migrated temporary database holding one plugin and one
    /// instance, and returns the store for the instance.
    async fn store(quota_bytes: u64) -> (tempfile::TempDir, Database, KvStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hearthsound.db");
        let database = Database::open(&path).await.unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();

        let plugin_id = "com.example.kv";
        let instance_id = new_uuid_v7();
        sqlx::query!(
            "INSERT INTO plugins (id, version, api, manifest, sha256, source, enabled, installed_at)
             VALUES (?, '0.1.0', 'hearthsound:plugin@0.2.0', '{}', X'', 'sideload', 1, 0)",
            plugin_id,
        )
        .execute(database.writer())
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO provider_instances
               (id, plugin_id, slug, display_name, enabled, created_at, updated_at)
             VALUES (?, ?, 'kv', 'KV', 1, 0, 0)",
            instance_id.as_bytes().as_slice(),
            plugin_id,
        )
        .execute(database.writer())
        .await
        .unwrap();

        let store = KvStore::new(database.writer().clone(), instance_id, quota_bytes);
        (dir, database, store)
    }

    #[tokio::test]
    async fn round_trips_a_value() {
        let (_dir, _db, store) = store(DEFAULT_QUOTA_BYTES).await;
        assert_eq!(store.get("missing").await.unwrap(), None);
        store.set("key", b"value", None).await.unwrap();
        assert_eq!(store.get("key").await.unwrap(), Some(b"value".to_vec()));
        store.set("key", b"other", None).await.unwrap();
        assert_eq!(store.get("key").await.unwrap(), Some(b"other".to_vec()));
        store.delete("key").await.unwrap();
        assert_eq!(store.get("key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_a_value_above_the_per_value_cap() {
        let (_dir, _db, store) = store(MAX_QUOTA_BYTES).await;
        let value = vec![0u8; (MAX_VALUE_BYTES + 1) as usize];
        assert!(matches!(
            store.set("key", &value, None).await,
            Err(KvError::ValueTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn refuses_to_exceed_the_quota() {
        let (_dir, _db, store) = store(MAX_VALUE_BYTES).await;
        store
            .set("a", &vec![0u8; MAX_VALUE_BYTES as usize], None)
            .await
            .unwrap();
        assert!(matches!(
            store
                .set("b", &vec![0u8; MAX_VALUE_BYTES as usize], None)
                .await,
            Err(KvError::QuotaExceeded { .. })
        ));
        // The refused write must not have evicted the stored key.
        assert_eq!(
            store.get("a").await.unwrap().map(|value| value.len()),
            Some(MAX_VALUE_BYTES as usize)
        );
    }

    #[tokio::test]
    async fn replacing_a_key_counts_only_the_new_value() {
        let (_dir, _db, store) = store(MAX_VALUE_BYTES).await;
        store
            .set("a", &vec![0u8; MAX_VALUE_BYTES as usize], None)
            .await
            .unwrap();
        store
            .set("a", &vec![0u8; MAX_VALUE_BYTES as usize], None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn honours_expiry_on_read() {
        let (_dir, _db, store) = store(DEFAULT_QUOTA_BYTES).await;
        store.set("key", b"value", Some(0)).await.unwrap();
        assert_eq!(store.get("key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn sweeps_expired_rows() {
        let (_dir, _db, store) = store(DEFAULT_QUOTA_BYTES).await;
        store.set("expired", b"value", Some(0)).await.unwrap();
        store.set("kept", b"value", None).await.unwrap();
        let removed = sweep_expired(&store.pool).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(sweep_expired(&store.pool).await.unwrap(), 0);
        assert!(store.get("kept").await.unwrap().is_some());
    }
}
