//! The SQLite storage layer.
//!
//! The database is opened as one dedicated writer connection and a small pool of
//! read-only reader connections. WAL mode lets readers run concurrently with the
//! single writer, so every write goes through [`Database::writer`] and every read
//! through [`Database::readers`]. Migrations are forward-only and are applied at
//! startup by [`Database::migrate`], which copies the database out of the way
//! before applying anything to a database that already holds data.
//!
//! Because a CLI command can write to the same file while the server runs, the
//! server watches for those commits with [`Database::watch_data_version`] and
//! reloads whatever the command changed.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

mod watch;

pub use watch::{DataVersionSubscription, DataVersionWatcher};

/// How many milliseconds a connection waits for a busy database before failing.
const BUSY_TIMEOUT_MS: u64 = 5000;

/// How many read-only connections the reader pool opens.
///
/// Eight, because every page read is several statements and a walk holds none of
/// them: the pool serves the pages a browser and a Subsonic client ask for at the
/// same time as the indexer's own reads.
pub const READER_CONNECTIONS: u32 = 8;

/// How many connections the writer pool opens.
///
/// One, because every write is serialized behind a single connection so write
/// transactions stay short and WAL checkpoint starvation is avoided.
pub const WRITER_CONNECTIONS: u32 = 1;

/// How often the server polls `PRAGMA data_version` for writes made by other
/// connections.
///
/// One second, so a change a CLI command makes takes effect about a second
/// later. See [`Database::watch_data_version`].
pub const DATA_VERSION_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The migrations bundled into this crate: the server's schema.
///
/// [`Database::migrate`] takes the migrator as a parameter so tests can apply
/// a migration set of their own.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// An open database with its writer and reader pools.
#[derive(Debug)]
pub struct Database {
    writer: SqlitePool,
    readers: SqlitePool,
    path: PathBuf,
}

/// What [`Database::migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOutcome {
    /// The migration version the database was at before migrating.
    pub from: i64,
    /// The migration version the database is at after migrating. Equal to
    /// `from` when nothing was pending.
    pub to: i64,
    /// The path of the pre-migration backup, when one was taken.
    pub backup: Option<PathBuf>,
}

impl Database {
    /// Opens the database at `path`, creating it when it does not exist.
    ///
    /// The writer pool is opened first so the file and its WAL mode exist before
    /// the read-only reader pool connects. Every connection enables foreign
    /// keys, a 5 second busy timeout, and `synchronous = NORMAL`.
    ///
    /// `NORMAL` is the WAL durability point worth paying for here: a commit is
    /// durable against a crash of the process and is never torn, but it is not
    /// forced to the disk on every commit. The alternative, `FULL`, fsyncs on
    /// every one of the thousands of writes an index pass makes, which is what
    /// made a walk hold the writer connection for seconds; the cost of `NORMAL`
    /// is that a power loss can lose the last few committed transactions while
    /// leaving the database consistent.
    ///
    /// # Parameters
    ///
    /// - `path`: the SQLite database file.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlx`] when the file cannot be opened or a pool cannot
    /// be created.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    ///
    /// # async fn example() -> Result<(), hearthsound_db::DbError> {
    /// let db = hearthsound_db::Database::open(Path::new("/tmp/hearthsound.db")).await?;
    /// db.close().await;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(path: &Path) -> Result<Database, DbError> {
        let base = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));

        let writer = SqlitePoolOptions::new()
            .max_connections(WRITER_CONNECTIONS)
            .connect_with(base.clone())
            .await?;

        let readers = SqlitePoolOptions::new()
            .max_connections(READER_CONNECTIONS)
            .connect_with(read_only_options(path))
            .await?;

        Ok(Database {
            writer,
            readers,
            path: path.to_path_buf(),
        })
    }

    /// Returns the path the database file was opened with.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the single writer pool.
    pub fn writer(&self) -> &SqlitePool {
        &self.writer
    }

    /// Returns the read-only reader pool.
    pub fn readers(&self) -> &SqlitePool {
        &self.readers
    }

    /// Closes both pools, waiting for in-flight statements to finish.
    pub async fn close(self) {
        self.writer.close().await;
        self.readers.close().await;
    }

    /// Applies pending migrations, after taking a backup when there is data to
    /// lose.
    ///
    /// A database that has never been migrated holds no data, so it is not
    /// backed up. A database that has data is copied with `VACUUM INTO` into
    /// `backup_dir` before the migrator runs.
    ///
    /// # Parameters
    ///
    /// - `migrator`: the migrations to apply.
    /// - `backup_dir`: the directory pre-migration backups are written to. It is
    ///   created when it does not exist.
    ///
    /// # Returns
    ///
    /// The version the database started at, the version it ended at, and the
    /// backup path when one was taken.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlx`] when the database cannot be read or written,
    /// [`DbError::BackupDir`] when the backup directory cannot be created, and
    /// [`DbError::Migrate`] when a migration fails. A failed migration leaves
    /// the backup in place as the recovery point.
    pub async fn migrate(
        &self,
        migrator: &Migrator,
        backup_dir: &Path,
    ) -> Result<MigrationOutcome, DbError> {
        refuse_legacy_schema(self.writer()).await?;
        let from = current_version(self.writer()).await?;
        let to = migrator
            .iter()
            .map(|migration| migration.version)
            .max()
            .unwrap_or(from);

        let mut backup = None;
        if to > from && from > 0 {
            std::fs::create_dir_all(backup_dir).map_err(|source| DbError::BackupDir {
                path: backup_dir.to_path_buf(),
                source,
            })?;
            let now = time::OffsetDateTime::now_utc();
            let millis = (now.unix_timestamp_nanos() / 1_000_000) as i64;
            let target = backup_dir.join(format!("hearthsound-{from}-to-{to}-{millis}.db"));
            sqlx::query("VACUUM INTO ?")
                .bind(target.to_string_lossy().into_owned())
                .execute(self.writer())
                .await?;
            backup = Some(target);
        }

        migrator.run(self.writer()).await?;

        Ok(MigrationOutcome { from, to, backup })
    }

    /// Reports the schema version the database is migrated to.
    ///
    /// The value is read from `_sqlx_migrations` over a reader connection, so it
    /// reflects the version a migrated database is actually at rather than what
    /// this process intended to apply. A readiness probe compares it with the
    /// version the bundled migrator supplies.
    ///
    /// # Returns
    ///
    /// The highest successfully applied migration version, or zero when the
    /// bookkeeping table does not exist yet, which is the case for a database
    /// file that was just created.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Sqlx`] when the query fails, which also covers a file
    /// that is not a SQLite database.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(db: &hearthsound_db::Database) -> Result<(), hearthsound_db::DbError> {
    /// assert_eq!(db.migration_version().await?, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn migration_version(&self) -> Result<i64, DbError> {
        current_version(self.readers()).await
    }
}

/// Returns the options a read-only connection to the database opens with.
///
/// The reader pool and the connection the data version is polled on share them,
/// so both see the same journal mode, busy timeout and foreign-key setting, and
/// neither can write.
///
/// # Parameters
///
/// - `path`: the database file.
///
/// # Returns
///
/// The options, with `create_if_missing` off: a reader never creates the file.
pub fn read_only_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .read_only(true)
}

/// Reads the highest successfully applied migration version.
///
/// Returns zero when the migration bookkeeping table does not exist yet, which
/// is the case for a brand new database file.
/// Refuses a database that was migrated past the baseline by an older release.
///
/// The baseline replaced every migration of the releases before v0.1.7, so a
/// database stamped with one of those versions holds a different schema at
/// version 1 and a completely different history above it. Neither can be
/// migrated here: the only supported path is a fresh data directory, and the
/// error says so.
///
/// # Parameters
///
/// - `pool`: the writer connection, which owns every migration write.
///
/// # Errors
///
/// Returns [`DbError::LegacySchema`] for a bookkeeping table that records a
/// version above 1, or version 1 under the old first migration's name, and
/// [`DbError::Sqlx`] when the table cannot be read.
async fn refuse_legacy_schema(pool: &SqlitePool) -> Result<(), DbError> {
    let has_table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if has_table == 0 {
        return Ok(());
    }
    let recorded: Option<(i64, String)> = sqlx::query_as(
        "SELECT MAX(version), COALESCE((SELECT description FROM _sqlx_migrations
                                         WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)),
                                       '')
         FROM _sqlx_migrations",
    )
    .fetch_optional(pool)
    .await?;
    match recorded {
        Some((version, _description)) if version > 1 => Err(DbError::LegacySchema),
        Some((version, description)) if version == 1 && description == "initial" => {
            Err(DbError::LegacySchema)
        }
        _ => Ok(()),
    }
}

async fn current_version(pool: &SqlitePool) -> Result<i64, DbError> {
    let has_table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if has_table == 0 {
        return Ok(0);
    }
    let version: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations WHERE success = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(version)
}

/// An error from the database layer.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The SQLite client reported an error.
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// A migration failed to apply.
    #[error("migration error: {0}")]
    Migrate(#[from] MigrateError),
    /// The database was created by a release older than the v0.1.7 baseline.
    #[error(
        "This database was created by a Hearthsound build older than the v0.1.7 baseline and cannot be upgraded. Stop the server, move the data directory aside and start with an empty one."
    )]
    LegacySchema,
    /// The backup directory could not be created.
    #[error("could not create backup directory `{path}`: {source}")]
    BackupDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}
