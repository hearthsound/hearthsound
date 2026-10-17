//! Per-user acquisition quotas.
//!
//! A user's `user_quotas` row caps how much that user may acquire in a rolling
//! 24-hour window: a number of acquisitions and a number of bytes. Both columns
//! are nullable and a `NULL` column means unlimited; a user with no row at all
//! is unlimited in both.
//!
//! The row is written and read here, and the acquisition layer is its consumer:
//! [`read`] takes a connection so admission can count the user's requests inside
//! the transaction it is already holding, while [`Quotas`] is the pool-level
//! repository the CLI and the API use to show and change a quota.
//!
//! The quota types hold unsigned numbers, so a negative quota cannot be
//! expressed; a value stored by other means that does not fit is reported as
//! [`AuthError::QuotaOutOfRange`] rather than silently read as unlimited.

use hearthsound_db::Database;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::error::AuthError;

/// A user's daily acquisition quotas.
///
/// `None` in either field means that dimension is unlimited. [`Default`] is
/// unlimited in both, which is also what a user with no `user_quotas` row is
/// given.
///
/// # Examples
///
/// ```
/// use hearthsound_auth::UserQuota;
///
/// assert!(UserQuota::default().is_unlimited());
/// let limited = UserQuota {
///     acquisitions_per_day: Some(10),
///     acquisition_bytes_per_day: None,
/// };
/// assert!(!limited.is_unlimited());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserQuota {
    /// How many acquisitions the user may start per rolling day, or `None` for
    /// no limit. Zero refuses every acquisition.
    pub acquisitions_per_day: Option<u32>,
    /// How many bytes of storage the user's acquisitions may account for per
    /// rolling day, or `None` for no limit.
    pub acquisition_bytes_per_day: Option<u64>,
}

impl UserQuota {
    /// Reports whether the quota limits nothing.
    ///
    /// # Returns
    ///
    /// `true` when both dimensions are unlimited, which is what
    /// [`Quotas::set`] stores by deleting the row.
    pub fn is_unlimited(&self) -> bool {
        self.acquisitions_per_day.is_none() && self.acquisition_bytes_per_day.is_none()
    }
}

/// Reads one user's quotas on a connection the caller owns.
///
/// The free function is what the acquisition layer uses: admission counts a
/// request inside its own write transaction, so it needs the quota from the
/// transaction's connection rather than from the pool.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user`: the account's id.
///
/// # Returns
///
/// The stored quota, or unlimited in both dimensions when the user has no row.
///
/// # Errors
///
/// Returns [`AuthError::QuotaOutOfRange`] when a stored value is negative or too
/// large for its column, and [`AuthError::Database`] when the read fails.
pub async fn read(conn: &mut SqliteConnection, user: Uuid) -> Result<UserQuota, AuthError> {
    let stored = sqlx::query!(
        "SELECT acquisitions_per_day, acquisition_bytes_per_day FROM user_quotas \
         WHERE user_id = ?",
        user.as_bytes().as_slice(),
    )
    .fetch_optional(&mut *conn)
    .await?;

    let Some(stored) = stored else {
        return Ok(UserQuota::default());
    };
    Ok(UserQuota {
        acquisitions_per_day: unsigned_u32(stored.acquisitions_per_day, "acquisitions_per_day")?,
        acquisition_bytes_per_day: unsigned_u64(
            stored.acquisition_bytes_per_day,
            "acquisition_bytes_per_day",
        )?,
    })
}

/// The `user_quotas` table.
///
/// Cloning is cheap: every clone shares the same two pools.
#[derive(Debug, Clone)]
pub struct Quotas {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

impl Quotas {
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
    /// let quotas = hearthsound_auth::Quotas::new(db);
    /// # let _ = quotas;
    /// # }
    /// ```
    pub fn new(db: &Database) -> Quotas {
        Quotas {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Reads one user's quotas.
    ///
    /// # Parameters
    ///
    /// - `user`: the account's id.
    ///
    /// # Returns
    ///
    /// The stored quota, or unlimited in both dimensions when the user has no
    /// row.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::QuotaOutOfRange`] when a stored value is negative or
    /// too large for its column, and [`AuthError::Database`] when the read
    /// fails.
    pub async fn get(&self, user: Uuid) -> Result<UserQuota, AuthError> {
        let mut conn = self.readers.acquire().await?;
        read(&mut conn, user).await
    }

    /// Stores one user's quotas.
    ///
    /// An unlimited quota deletes the row, which is how it was stored before the
    /// user had one; every other quota is written with an upsert. The value is
    /// applied as a whole, so a quota that limits only one dimension clears the
    /// other.
    ///
    /// # Parameters
    ///
    /// - `user`: the account's id. The row is a foreign key, so an unknown user
    ///   is a database error.
    /// - `quota`: the limits to store.
    ///
    /// # Returns
    ///
    /// `Ok(())` once the row is written or deleted.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::QuotaOutOfRange`] when a byte quota is larger than
    /// the column can hold, and [`AuthError::Database`] when the write fails.
    pub async fn set(&self, user: Uuid, quota: &UserQuota) -> Result<(), AuthError> {
        if quota.is_unlimited() {
            sqlx::query!(
                "DELETE FROM user_quotas WHERE user_id = ?",
                user.as_bytes().as_slice(),
            )
            .execute(&self.writer)
            .await?;
            return Ok(());
        }

        let acquisitions = quota.acquisitions_per_day.map(i64::from);
        let bytes = quota
            .acquisition_bytes_per_day
            .map(|bytes| {
                i64::try_from(bytes).map_err(|_| AuthError::QuotaOutOfRange {
                    column: "acquisition_bytes_per_day",
                })
            })
            .transpose()?;

        sqlx::query!(
            "INSERT INTO user_quotas (user_id, acquisitions_per_day, acquisition_bytes_per_day) \
             VALUES (?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET \
             acquisitions_per_day = excluded.acquisitions_per_day, \
             acquisition_bytes_per_day = excluded.acquisition_bytes_per_day",
            user.as_bytes().as_slice(),
            acquisitions,
            bytes,
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }
}

/// Converts a stored acquisitions-per-day value.
///
/// # Parameters
///
/// - `stored`: the column's value.
/// - `column`: the column's name, for the error.
///
/// # Returns
///
/// The value, or `None` for a stored `NULL`.
///
/// # Errors
///
/// Returns [`AuthError::QuotaOutOfRange`] for a negative or too large value.
fn unsigned_u32(stored: Option<i64>, column: &'static str) -> Result<Option<u32>, AuthError> {
    stored
        .map(|value| u32::try_from(value).map_err(|_| AuthError::QuotaOutOfRange { column }))
        .transpose()
}

/// Converts a stored acquisition-bytes-per-day value.
///
/// # Parameters
///
/// - `stored`: the column's value.
/// - `column`: the column's name, for the error.
///
/// # Returns
///
/// The value, or `None` for a stored `NULL`.
///
/// # Errors
///
/// Returns [`AuthError::QuotaOutOfRange`] for a negative value.
fn unsigned_u64(stored: Option<i64>, column: &'static str) -> Result<Option<u64>, AuthError> {
    stored
        .map(|value| u64::try_from(value).map_err(|_| AuthError::QuotaOutOfRange { column }))
        .transpose()
}

#[cfg(test)]
mod tests {
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

    /// Inserts an account and returns its id.
    async fn account(db: &Database, username: &str) -> Uuid {
        let id = hearthsound_core::new_uuid_v7();
        sqlx::query!(
            "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
             VALUES (?, ?, ?, 'x', 0, 0)",
            id.as_bytes().as_slice(),
            username,
            username,
        )
        .execute(db.writer())
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn a_user_without_a_row_is_unlimited() {
        let (_dir, db) = database().await;
        let user = account(&db, "echo").await;
        let quotas = Quotas::new(&db);

        assert_eq!(quotas.get(user).await.unwrap(), UserQuota::default());
        assert!(quotas.get(user).await.unwrap().is_unlimited());
    }

    #[tokio::test]
    async fn a_quota_round_trips_and_clears_the_other_dimension() {
        let (_dir, db) = database().await;
        let user = account(&db, "echo").await;
        let quotas = Quotas::new(&db);

        let both = UserQuota {
            acquisitions_per_day: Some(20),
            acquisition_bytes_per_day: Some(5_000_000_000),
        };
        quotas.set(user, &both).await.unwrap();
        assert_eq!(quotas.get(user).await.unwrap(), both);

        let count_only = UserQuota {
            acquisitions_per_day: Some(3),
            acquisition_bytes_per_day: None,
        };
        quotas.set(user, &count_only).await.unwrap();
        assert_eq!(quotas.get(user).await.unwrap(), count_only);

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_quotas")
            .fetch_one(db.writer())
            .await
            .unwrap();
        assert_eq!(rows, 1, "one row per user");

        quotas.set(user, &UserQuota::default()).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_quotas")
            .fetch_one(db.writer())
            .await
            .unwrap();
        assert_eq!(rows, 0, "an unlimited quota is stored as no row");
    }

    #[tokio::test]
    async fn a_zero_count_quota_is_stored_as_zero() {
        let (_dir, db) = database().await;
        let user = account(&db, "echo").await;
        let quotas = Quotas::new(&db);

        let refused = UserQuota {
            acquisitions_per_day: Some(0),
            acquisition_bytes_per_day: None,
        };
        quotas.set(user, &refused).await.unwrap();

        assert_eq!(quotas.get(user).await.unwrap(), refused);
        assert!(!quotas.get(user).await.unwrap().is_unlimited());
    }

    #[tokio::test]
    async fn quotas_are_per_user() {
        let (_dir, db) = database().await;
        let echo = account(&db, "echo").await;
        let zoe = account(&db, "zoe").await;
        let quotas = Quotas::new(&db);

        quotas
            .set(
                echo,
                &UserQuota {
                    acquisitions_per_day: Some(1),
                    acquisition_bytes_per_day: None,
                },
            )
            .await
            .unwrap();

        let echo_quota = quotas.get(echo).await.unwrap();
        assert_eq!(echo_quota.acquisitions_per_day, Some(1));
        assert!(quotas.get(zoe).await.unwrap().is_unlimited());
    }

    #[tokio::test]
    async fn a_negative_stored_value_is_reported() {
        let (_dir, db) = database().await;
        let user = account(&db, "echo").await;

        sqlx::query!(
            "INSERT INTO user_quotas (user_id, acquisitions_per_day, acquisition_bytes_per_day) \
             VALUES (?, -1, NULL)",
            user.as_bytes().as_slice(),
        )
        .execute(db.writer())
        .await
        .unwrap();

        let error = Quotas::new(&db).get(user).await.unwrap_err();
        assert!(matches!(
            error,
            AuthError::QuotaOutOfRange {
                column: "acquisitions_per_day"
            }
        ));
    }

    #[tokio::test]
    async fn a_quota_for_an_unknown_user_is_a_foreign_key_error() {
        let (_dir, db) = database().await;

        let error = Quotas::new(&db)
            .set(
                hearthsound_core::new_uuid_v7(),
                &UserQuota {
                    acquisitions_per_day: Some(1),
                    acquisition_bytes_per_day: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Database(_)));
    }
}
