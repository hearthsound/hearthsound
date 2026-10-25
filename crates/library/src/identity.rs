//! The one-time library passes, and the identity consolidation they run.
//!
//! A scan applies the identity rules of [`crate::artists`] and
//! [`crate::albums`] to the files it reads, but an incremental scan skips every
//! unchanged file, so rows that an older version created - an artist merged with
//! a namesake, an album whose year the last scan replaced - would never be
//! looked at again. The `library.reidentity` job is that repair: it scans every
//! folder in full, which re-probes every file and rewrites each track's album and
//! credits from the tags it actually carries, and then merges the duplicate rows
//! that are left.
//!
//! The pass runs once. [`enqueue_if_pending`] is called when the server starts:
//! it does nothing when the pass already completed or when the server holds no
//! folders, and otherwise enqueues one [`REIDENTITY_JOB_KIND`] job. The job
//! writes a [`library_maintenance`](REIDENTITY_PASS) marker only when every
//! folder scanned, so a start that could not read a disk runs the pass again;
//! a folder whose root is empty does not block the marker, because there is
//! nothing in it to re-evaluate.
//!
//! What the pass can repair is what the current tags say: a track that is still
//! on disk moves to the artist and album its tags name. What it cannot repair is
//! data that was already lost - a year that an earlier scan overwrote stays as it
//! is, and the tag that first merged two artists is not recorded anywhere.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use hearthsound_core::{IdPrefix, Priority};
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler, JobQueue, NewJob};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::decode_entity;
use crate::scan::{LibraryScanner, ScanRequest};
use crate::{LibraryRepository, READ_THEN_WRITE, albums, artists, now_millis};

/// The name of the identity pass in the `library_maintenance` table.
pub const REIDENTITY_PASS: &str = "reidentity-1";

/// The job kind the identity pass runs under.
pub const REIDENTITY_JOB_KIND: &str = "library.reidentity";

/// Reports whether a one-time pass has not completed yet.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `name`: the pass's name, for example [`REIDENTITY_PASS`].
///
/// # Returns
///
/// `true` when no completed row exists for that name.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
///
/// # Examples
///
/// ```no_run
/// use hearthsound_library::LibraryError;
/// use hearthsound_library::identity::{REIDENTITY_PASS, maintenance_pending};
///
/// # async fn example(db: &hearthsound_db::Database) -> Result<(), LibraryError> {
/// if maintenance_pending(db, REIDENTITY_PASS).await? {
///     println!("the identity pass still has to run");
/// }
/// # Ok(())
/// # }
/// ```
pub async fn maintenance_pending(database: &Database, name: &str) -> Result<bool, LibraryError> {
    let mut read = database.readers().acquire().await?;
    let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM library_maintenance WHERE name = ?")
        .bind(name)
        .fetch_optional(&mut *read)
        .await?;
    Ok(found.is_none())
}

/// Records that a one-time pass completed.
///
/// The write is idempotent: a pass that runs twice for one reason or another
/// keeps the first completion time.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `name`: the pass's name.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails.
pub async fn mark_maintenance_complete(
    database: &Database,
    name: &str,
) -> Result<(), LibraryError> {
    sqlx::query(
        "INSERT INTO library_maintenance (name, completed_at) VALUES (?, ?) \
         ON CONFLICT (name) DO NOTHING",
    )
    .bind(name)
    .bind(now_millis())
    .execute(database.writer())
    .await?;
    Ok(())
}

/// Enqueues the identity pass when it has not run and there is anything to run it
/// on.
///
/// A fresh install holds no folder, so nothing is enqueued: the pass would have
/// nothing to repair and the rows a scan writes already follow the rule.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `queue`: the queue to enqueue on.
///
/// # Returns
///
/// `true` when the pass was enqueued, which means the caller can leave the
/// per-folder startup scan to it.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a read fails and
/// [`LibraryError::Queue`] when the job cannot be written.
pub async fn enqueue_if_pending(
    database: &Database,
    queue: &JobQueue,
) -> Result<bool, LibraryError> {
    if !maintenance_pending(database, REIDENTITY_PASS).await? {
        return Ok(false);
    }
    let folders = LibraryRepository::new(database)
        .list_library_folders()
        .await?;
    if folders.is_empty() {
        return Ok(false);
    }
    queue
        .enqueue(NewJob::new(REIDENTITY_JOB_KIND).priority(Priority::Background))
        .await?;
    tracing::info!("queued the library identity pass");
    Ok(true)
}

/// Scans every folder in full and merges the duplicate rows that remain.
///
/// The folder scans reuse the normal path, so `scan.progress` events and the
/// `last_scan_at`/`last_scan_error` bookkeeping happen as for any scan. A folder
/// that cannot be scanned is logged and leaves the pass pending; a folder whose
/// scan refused to prune counts as scanned, because an empty root has nothing to
/// re-evaluate.
#[derive(Clone)]
pub struct ReidentityJobHandler {
    /// The scanner every folder is re-probed with.
    scanner: Arc<LibraryScanner>,
    /// The database the rows are consolidated in.
    database: Arc<Database>,
}

impl ReidentityJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `scanner`: the scanner to scan every folder with.
    /// - `database`: the open database.
    ///
    /// # Returns
    ///
    /// The handler.
    pub fn new(scanner: Arc<LibraryScanner>, database: Arc<Database>) -> ReidentityJobHandler {
        ReidentityJobHandler { scanner, database }
    }
}

#[async_trait]
impl JobHandler for ReidentityJobHandler {
    /// Runs one identity pass.
    ///
    /// The job can be claimed twice - a restart mid-pass, a lock that expired -
    /// and the marker is what makes the second claim a no-op.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job.
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the marker cannot be read, a merge fails, or the
    /// marker cannot be written.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let failed = |error: LibraryError| JobError::new(error.to_string());
        if !maintenance_pending(&self.database, REIDENTITY_PASS)
            .await
            .map_err(failed)?
        {
            tracing::debug!(job = %job.id, "the library identity pass has already completed");
            return Ok(());
        }

        let folders = LibraryRepository::new(&self.database)
            .list_library_folders()
            .await
            .map_err(failed)?;
        let mut failed_folders = 0_usize;
        for folder in &folders {
            let request = ScanRequest::folder(folder.id).full();
            if let Err(error) = self.scanner.scan(&request).await {
                failed_folders += 1;
                tracing::warn!(
                    folder = %folder.id,
                    path = %folder.path.display(),
                    %error,
                    "the library identity pass could not scan a folder"
                );
            }
        }

        let artists = consolidate_artists(&self.database).await.map_err(failed)?;
        let albums = consolidate_albums(&self.database).await.map_err(failed)?;
        tracing::info!(artists, albums, "merged duplicate artists and albums");

        if failed_folders == 0 {
            mark_maintenance_complete(&self.database, REIDENTITY_PASS)
                .await
                .map_err(failed)?;
        } else {
            tracing::warn!(
                folders = failed_folders,
                "the library identity pass runs again on the next start because some folders \
                 could not be scanned"
            );
        }
        Ok(())
    }
}

/// Merges the artist rows that the identity rule says are one artist.
///
/// Rows that share an identifier collapse into the oldest of them, and then the
/// unambiguously named untagged rows join their artist: exactly one identified
/// artist of the name takes every untagged row, and otherwise the untagged rows
/// become one row of their own. An identified row is never merged away without an
/// identifier match.
///
/// Each name is one transaction, so the writer is never held for the whole pass.
///
/// # Parameters
///
/// - `database`: the open database.
///
/// # Returns
///
/// How many rows were merged away.
///
/// # Errors
///
/// Returns [`LibraryError`] when a read or a write fails.
pub async fn consolidate_artists(database: &Database) -> Result<u64, LibraryError> {
    let names: Vec<String> = {
        let mut read = database.readers().acquire().await?;
        sqlx::query_scalar(
            "SELECT name FROM artists GROUP BY name COLLATE NOCASE HAVING COUNT(*) > 1",
        )
        .fetch_all(&mut *read)
        .await?
    };
    let mut merged = 0_u64;
    for name in names {
        let group = {
            let mut read = database.readers().acquire().await?;
            artists::name_group(&mut read, &name).await?
        };
        if group.len() < 2 {
            continue;
        }
        let mut tx = database.writer().begin_with(READ_THEN_WRITE).await?;
        merged += merge_artist_group(&mut tx, group).await?;
        tx.commit().await?;
    }
    Ok(merged)
}

/// Merges one name group, oldest row first.
///
/// # Parameters
///
/// - `conn`: the transaction the group is merged in.
/// - `group`: the rows of one name, oldest first.
///
/// # Returns
///
/// How many rows were merged away.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
async fn merge_artist_group(
    conn: &mut SqliteConnection,
    group: Vec<artists::NameGroupRow>,
) -> Result<u64, LibraryError> {
    let mut identified: HashSet<Uuid> = group
        .iter()
        .filter(|row| row.identified())
        .map(|row| row.id)
        .collect();
    let mut alive: Vec<Uuid> = group.iter().map(|row| row.id).collect();
    let mut merged = 0_u64;

    // Every identifier is a key that links the rows holding it. The MusicBrainz
    // column is one key per row, and every alias row is another.
    let mut by_code: HashMap<(String, String), Vec<Uuid>> = HashMap::new();
    for row in &group {
        if let Some(mbid) = &row.mb_artist_id {
            by_code
                .entry(("mb-artist".to_owned(), mbid.clone()))
                .or_default()
                .push(row.id);
        }
        let aliases: Vec<(String, String)> = sqlx::query_as(
            "SELECT namespace, value FROM external_ids \
             WHERE entity_kind = 'artist' AND entity_id = ?",
        )
        .bind(row.id.as_bytes().as_slice())
        .fetch_all(&mut *conn)
        .await?;
        for alias in aliases {
            by_code.entry(alias).or_default().push(row.id);
        }
    }
    for rows in by_code.values() {
        let Some((survivor, rest)) = rows.split_first() else {
            continue;
        };
        for loser in rest {
            if !alive.contains(loser) || !alive.contains(survivor) {
                continue;
            }
            artists::merge_artist(conn, *loser, *survivor).await?;
            alive.retain(|id| id != loser);
            if identified.remove(loser) {
                identified.insert(*survivor);
            }
            merged += 1;
        }
    }

    let tagged: Vec<Uuid> = alive
        .iter()
        .copied()
        .filter(|id| identified.contains(id))
        .collect();
    let untagged: Vec<Uuid> = alive
        .iter()
        .copied()
        .filter(|id| !identified.contains(id))
        .collect();
    let target = if tagged.len() == 1 {
        tagged.first().copied()
    } else {
        untagged.first().copied()
    };
    if let Some(target) = target {
        for loser in alive.clone() {
            if loser == target {
                continue;
            }
            // An identified row is never merged away without an identifier
            // match; only the untagged rows of the group move.
            if identified.contains(&loser) {
                continue;
            }
            artists::merge_artist(conn, loser, target).await?;
            merged += 1;
        }
    }
    Ok(merged)
}

/// Merges the album rows that the identity rule says are one album.
///
/// Rows that share a release id collapse into the oldest of them, and then the
/// id-less rows of the identity join their album: exactly one identified album
/// of the identity takes every id-less row, and otherwise the id-less rows become
/// one row of their own. An album without a credit at position 0 has no identity
/// to compare, so it is skipped.
///
/// Each identity is one transaction, so the writer is never held for the whole
/// pass.
///
/// # Parameters
///
/// - `database`: the open database.
///
/// # Returns
///
/// How many rows were merged away.
///
/// # Errors
///
/// Returns [`LibraryError`] when a read or a write fails.
pub async fn consolidate_albums(database: &Database) -> Result<u64, LibraryError> {
    let groups: Vec<(String, Vec<u8>, Option<i64>)> = {
        let mut read = database.readers().acquire().await?;
        sqlx::query_as(
            "SELECT al.sort_title, aa.artist_id, al.year \
               FROM albums al JOIN album_artists aa ON aa.album_id = al.id AND aa.position = 0 \
              GROUP BY al.sort_title, aa.artist_id, al.year \
             HAVING COUNT(*) > 1",
        )
        .fetch_all(&mut *read)
        .await?
    };
    let mut merged = 0_u64;
    let mut skipped = 0_usize;
    for (sort_title, artist_key, year) in groups {
        let Ok(primary) = decode_entity(IdPrefix::Artist, &artist_key) else {
            skipped += 1;
            continue;
        };
        let group = {
            let mut read = database.readers().acquire().await?;
            albums::identity_group(&mut read, &sort_title, primary, year).await?
        };
        if group.len() < 2 {
            continue;
        }
        let mut tx = database.writer().begin_with(READ_THEN_WRITE).await?;
        merged += merge_album_group(&mut tx, group).await?;
        tx.commit().await?;
    }
    if skipped > 0 {
        tracing::debug!(
            skipped,
            "skipped album groups whose primary credit is not an artist"
        );
    }
    Ok(merged)
}

/// Merges one album identity group, oldest row first.
///
/// # Parameters
///
/// - `conn`: the transaction the group is merged in.
/// - `group`: the rows of one identity, oldest first.
///
/// # Returns
///
/// How many rows were merged away.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
async fn merge_album_group(
    conn: &mut SqliteConnection,
    group: Vec<albums::AlbumIdentityRow>,
) -> Result<u64, LibraryError> {
    let mut identified: HashSet<Uuid> = group
        .iter()
        .filter(|row| row.mb_release_id.is_some())
        .map(|row| row.id)
        .collect();
    let mut alive: Vec<Uuid> = group.iter().map(|row| row.id).collect();
    let mut merged = 0_u64;

    let mut by_release: HashMap<String, Vec<Uuid>> = HashMap::new();
    for row in &group {
        if let Some(release) = &row.mb_release_id {
            by_release.entry(release.clone()).or_default().push(row.id);
        }
    }
    for rows in by_release.values() {
        let Some((survivor, rest)) = rows.split_first() else {
            continue;
        };
        for loser in rest {
            if !alive.contains(loser) || !alive.contains(survivor) {
                continue;
            }
            albums::merge_album(conn, *loser, *survivor).await?;
            alive.retain(|id| id != loser);
            if identified.remove(loser) {
                identified.insert(*survivor);
            }
            merged += 1;
        }
    }

    let tagged: Vec<Uuid> = alive
        .iter()
        .copied()
        .filter(|id| identified.contains(id))
        .collect();
    let untagged: Vec<Uuid> = alive
        .iter()
        .copied()
        .filter(|id| !identified.contains(id))
        .collect();
    let target = if tagged.len() == 1 {
        tagged.first().copied()
    } else {
        untagged.first().copied()
    };
    if let Some(target) = target {
        for loser in alive.clone() {
            if loser == target || identified.contains(&loser) {
                continue;
            }
            albums::merge_album(conn, loser, target).await?;
            merged += 1;
        }
    }
    Ok(merged)
}
