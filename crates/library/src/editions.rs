//! The edition label of a local album, and the one-time pass that fills it in.
//!
//! Two releases of one title by one artist are two albums: the standard release
//! and the deluxe edition share their title, so nothing in a grid tells them
//! apart. The label is what does, and it is derived from what the release itself
//! says, in this precedence:
//!
//! 1. the release's disambiguation comment, as MusicBrainz Picard writes it
//!    (`MUSICBRAINZ_ALBUMCOMMENT`, `MusicBrainz Album Comment` in an APE item or
//!    an ID3v2 `TXXX` frame, and the `com.apple.iTunes` freeform atom of an MP4
//!    file), which [`hearthsound_media::ProbeResult::release_comment`] reads;
//! 2. the album's folder, when its files share one parent directory whose name
//!    starts with the album title and continues past it: `Solar Power (Deluxe
//!    Edition)` under the title `Solar Power` is `Deluxe Edition`.
//!
//! The scanner derives the label for every file it probes, so it stays current;
//! [`EDITIONS_PASS`] is the one-time pass that fills it in for the rows a library
//! already holds, without the owner touching a file.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use hearthsound_core::{EntityId, IdPrefix, Priority};
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler, JobQueue, NewJob};

use crate::error::LibraryError;
use crate::model::{EditionInput, decode_entity, entity_key};

/// The name of the edition backfill in the `library_maintenance` table.
pub const EDITIONS_PASS: &str = "editions-1";

/// The job kind the edition backfill runs under.
pub const EDITIONS_JOB_KIND: &str = "library.editions";

/// The characters that surround an edition in a folder name and are not part of
/// it: brackets and parentheses of every shape, dashes, quotes, and the
/// separators a tagger leaves behind.
const EDITION_EDGES: &[char] = &[
    '(', ')', '[', ']', '{', '}', '<', '>', '-', '\u{2013}', '\u{2014}', ':', ',', '.', '\'', '"',
    '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
];

/// Derives the edition label of an album from its folder name.
///
/// The folder is compared to the title case- and punctuation-insensitively, so
/// `SOLAR POWER (Deluxe Edition)` under the title `Solar Power` matches; the
/// label is what follows the title, stripped of the brackets, parentheses,
/// dashes, quotes and whitespace that surrounded it. A folder that is the title
/// and nothing more yields `None` - it states no edition - and so does a folder
/// that does not start with the title, which is a different album or an artist
/// folder.
///
/// # Parameters
///
/// - `title`: the album's title, as the tags spell it.
/// - `folder`: the name of the directory the album's files live in.
///
/// # Returns
///
/// The label, or `None` when the folder states none.
///
/// # Examples
///
/// ```
/// use hearthsound_library::editions::label_from_folder;
///
/// assert_eq!(
///     label_from_folder("Solar Power", "Solar Power (Deluxe Edition)").as_deref(),
///     Some("Deluxe Edition"),
/// );
/// assert_eq!(label_from_folder("Solar Power", "Solar Power"), None);
/// assert_eq!(label_from_folder("Solar Power", "Melodrama"), None);
/// ```
pub fn label_from_folder(title: &str, folder: &str) -> Option<String> {
    let title: Vec<char> = folded_alphanumerics(title).collect();
    if title.is_empty() {
        return None;
    }
    let mut matched = 0_usize;
    let mut split = None;
    for (index, character) in folder.char_indices() {
        for folded in folded_alphanumerics(&character.to_string()) {
            if folded != title[matched] {
                return None;
            }
            matched += 1;
            if matched == title.len() {
                split = Some(index + character.len_utf8());
                break;
            }
        }
        if split.is_some() {
            break;
        }
    }
    let remainder = strip_edges(&folder[split?..]);
    remainder
        .chars()
        .any(char::is_alphanumeric)
        .then_some(remainder)
}

/// Returns the name of the directory a relative path leaves.
///
/// # Parameters
///
/// - `rel_path`: the path relative to the folder root, with `/` separators.
///
/// # Returns
///
/// The directory's own name, or `None` when the file sits at the root or its
/// parent's name is not UTF-8.
///
/// # Examples
///
/// ```
/// use hearthsound_library::editions::parent_folder;
///
/// assert_eq!(
///     parent_folder("Lorde/Solar Power (Deluxe Edition)/01.flac"),
///     Some("Solar Power (Deluxe Edition)"),
/// );
/// assert_eq!(parent_folder("01.flac"), None);
/// ```
pub fn parent_folder(rel_path: &str) -> Option<&str> {
    let parent = std::path::Path::new(rel_path).parent()?;
    (!parent.as_os_str().is_empty())
        .then(|| parent.file_name().and_then(|name| name.to_str()))
        .flatten()
}

/// Derives the label of an album from everything its files state about it.
///
/// The release comment wins over the folder, because it is the release's own
/// statement of what it is; the folder is a guess from the owner's naming. The
/// folder rule needs the album's files to share one directory - a multi-disc
/// release spreads them over `CD1` and `CD2`, and neither names an edition - so
/// more than one parent states no label. A file at the folder root has no parent
/// name and states none either.
///
/// # Parameters
///
/// - `title`: the album's title.
/// - `parents`: the parent directory names the album's files sit in, `""` for a
///   file at the folder root.
/// - `comment`: the release comment the files carry, when any of them does.
///
/// # Returns
///
/// What the writer states about the album's label.
pub(crate) fn album_input(
    title: &str,
    parents: &BTreeSet<String>,
    comment: Option<&str>,
) -> EditionInput {
    if let Some(comment) = label(comment) {
        return EditionInput::Labelled(comment);
    }
    let mut parents = parents.iter();
    match (parents.next(), parents.next()) {
        (Some(folder), None) => {
            label_from_folder(title, folder).map_or(EditionInput::Absent, EditionInput::Labelled)
        }
        _ => EditionInput::Absent,
    }
}

/// States a release comment as a label and states nothing when there is none.
///
/// This is the upload path's rule: a stored file is named by the server's own
/// template, so the directory it lands in says nothing about the release, and only
/// the file's own comment does.
///
/// # Parameters
///
/// - `comment`: the release comment the file carries.
///
/// # Returns
///
/// [`EditionInput::Labelled`] for a comment, [`EditionInput::Unstated`] otherwise.
pub(crate) fn comment_input(comment: Option<&str>) -> EditionInput {
    label(comment).map_or(EditionInput::Unstated, EditionInput::Labelled)
}

/// Normalises a release comment into a label.
///
/// # Parameters
///
/// - `comment`: the comment the file carries, when it carries one.
///
/// # Returns
///
/// The trimmed comment when it holds anything, `None` otherwise.
fn label(comment: Option<&str>) -> Option<String> {
    comment
        .map(str::trim)
        .filter(|comment| !comment.is_empty())
        .map(str::to_owned)
}

/// Iterates the characters of a string that survive the edition comparison.
///
/// A comparison of folder against title ignores case and punctuation: it sums
/// the letters and digits only, lowercased.
///
/// # Parameters
///
/// - `text`: the text to fold.
///
/// # Returns
///
/// The letters and digits, lowercased.
fn folded_alphanumerics(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
}

/// Strips the brackets, parentheses, dashes, quotes and whitespace that
/// surround a leftover, and collapses the whitespace inside it.
///
/// # Parameters
///
/// - `remainder`: what followed the title in the folder name.
///
/// # Returns
///
/// The stripped remainder, which is empty when it held nothing else.
fn strip_edges(remainder: &str) -> String {
    let trimmed = remainder.trim().trim_matches(|character: char| {
        EDITION_EDGES.contains(&character) || character.is_whitespace()
    });
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One album the backfill derives a label for.
#[derive(Debug)]
struct AlbumFiles {
    /// The album's client id.
    id: EntityId,
    /// The album's title, as the tags spell it.
    title: String,
    /// The label the row stores now.
    stored: Option<String>,
    /// The directories the album's files sit in, by name; empty for the root.
    parents: BTreeSet<String>,
    /// The first file of the album, absolute and most preferred first, which is
    /// the one the tag rule is read from.
    preferred: PathBuf,
}

/// Fills in the edition label of every local album a library already holds.
///
/// The scanner derives the label for every file it probes, so a library that is
/// scanned from now on needs nothing. Rows that existed before the label did are
/// what this fills in, and it does so without the owner touching a file: the
/// folder rule is read from `media_files.rel_path`, and the tag rule from one file
/// per album - the release's disambiguation is a property of the release, so one
/// file states it for all of them.
///
/// A file that cannot be probed leaves the album's stored label alone rather than
/// clearing it: a disk that is not mounted must not strip a library's labels.
///
/// # Parameters
///
/// - `database`: the open database.
///
/// # Returns
///
/// How many album rows the pass wrote.
///
/// # Errors
///
/// Returns [`LibraryError`] when a read or a write fails.
async fn backfill(database: &Database) -> Result<u64, LibraryError> {
    let mut collected: HashMap<EntityId, AlbumFiles> = HashMap::new();
    {
        let mut read = database.readers().acquire().await?;
        let rows = sqlx::query!(
            "SELECT a.id AS album_id, a.title AS title, a.edition AS edition, \
                    f.path AS folder_path, mf.rel_path AS rel_path \
               FROM albums a \
               JOIN tracks t ON t.album_id = a.id \
               JOIN media_files mf ON mf.track_id = t.id AND mf.storage = 'folder' \
               JOIN library_folders f ON f.id = mf.folder_id \
              WHERE a.origin = 'local' \
              ORDER BY a.id, mf.is_preferred DESC, mf.added_at, mf.rel_path",
        )
        .fetch_all(&mut *read)
        .await?;
        for row in rows {
            let id = decode_entity(IdPrefix::Album, &row.album_id)?;
            let absolute = Path::new(&row.folder_path).join(&row.rel_path);
            // The rows arrive most preferred file first, so the first one
            // inserted is the file the tag rule is read from.
            let entry = collected.entry(id).or_insert_with(|| AlbumFiles {
                id,
                title: row.title.clone(),
                stored: row.edition.clone(),
                parents: BTreeSet::new(),
                preferred: absolute.clone(),
            });
            entry
                .parents
                .insert(parent_folder(&row.rel_path).unwrap_or_default().to_owned());
        }
    }

    let mut written = 0_u64;
    for album in collected.values() {
        let comment = hearthsound_media::probe(&album.preferred)
            .await
            .ok()
            .and_then(|probe| probe.release_comment);
        let stated = album_input(&album.title, &album.parents, comment.as_deref());
        let label = stated.label();
        if label == album.stored.as_deref() {
            continue;
        }
        sqlx::query("UPDATE albums SET edition = ?, updated_at = ? WHERE id = ?")
            .bind(label)
            .bind(crate::now_millis())
            .bind(entity_key(album.id))
            .execute(database.writer())
            .await?;
        written += 1;
    }
    tracing::info!(
        albums = written,
        "labelled the editions of the stored albums"
    );
    Ok(written)
}

/// Enqueues the edition backfill when it has not run yet.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `queue`: the queue to enqueue on.
///
/// # Returns
///
/// `true` when the pass was enqueued.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a read fails and
/// [`LibraryError::Queue`] when the job cannot be written.
pub async fn enqueue_if_pending(
    database: &Database,
    queue: &JobQueue,
) -> Result<bool, LibraryError> {
    if !crate::identity::maintenance_pending(database, EDITIONS_PASS).await? {
        return Ok(false);
    }
    queue
        .enqueue(NewJob::new(EDITIONS_JOB_KIND).priority(Priority::Background))
        .await?;
    tracing::info!("queued the album edition backfill");
    Ok(true)
}

/// Fills in the edition labels of a library that predates them.
///
/// The pass runs once. A filesystem that cannot be read leaves the marker unwritten
/// so the pass runs again on the next start; the labels it did write stay.
#[derive(Clone)]
pub struct EditionsJobHandler {
    /// The database the labels are read from and written to.
    database: Arc<Database>,
}

impl EditionsJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    ///
    /// # Returns
    ///
    /// The handler.
    pub fn new(database: Arc<Database>) -> EditionsJobHandler {
        EditionsJobHandler { database }
    }
}

#[async_trait]
impl JobHandler for EditionsJobHandler {
    /// Runs one edition backfill.
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
    /// Returns [`JobError`] when the marker cannot be read or written, or the
    /// labels cannot be written.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let failed = |error: LibraryError| JobError::new(error.to_string());
        if !crate::identity::maintenance_pending(&self.database, EDITIONS_PASS)
            .await
            .map_err(failed)?
        {
            tracing::debug!(job = %job.id, "the album edition backfill has already completed");
            return Ok(());
        }
        let written = backfill(&self.database).await.map_err(failed)?;
        crate::identity::mark_maintenance_complete(&self.database, EDITIONS_PASS)
            .await
            .map_err(failed)?;
        tracing::info!(albums = written, "the album edition backfill completed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{album_input, comment_input, label_from_folder, parent_folder};
    use crate::model::EditionInput;

    #[test]
    fn a_bracketed_edition_is_the_remainder() {
        assert_eq!(
            label_from_folder("Solar Power", "Solar Power (Deluxe Edition)").as_deref(),
            Some("Deluxe Edition")
        );
    }

    #[test]
    fn the_comparison_ignores_case_and_punctuation() {
        assert_eq!(
            label_from_folder("Solar Power", "SOLAR POWER - deluxe edition").as_deref(),
            Some("deluxe edition")
        );
        assert_eq!(
            label_from_folder("Twin Fantasy", "Twin Fantasy [Face to Face]").as_deref(),
            Some("Face to Face")
        );
    }

    #[test]
    fn a_folder_that_is_the_title_states_no_edition() {
        assert_eq!(label_from_folder("Solar Power", "Solar Power"), None);
        assert_eq!(label_from_folder("Solar Power", "Solar Power ( )"), None);
        assert_eq!(label_from_folder("Solar Power", "Solar Power!"), None);
    }

    #[test]
    fn a_folder_that_is_not_the_title_states_no_edition() {
        assert_eq!(label_from_folder("Solar Power", "Melodrama"), None);
        assert_eq!(
            label_from_folder("Teen of Denial (Joe's Story)", "Teens of Denial"),
            None
        );
        assert_eq!(label_from_folder("Solar Power", "Solar"), None);
        assert_eq!(label_from_folder("Solar Power", ""), None);
    }

    #[test]
    fn the_parent_of_a_root_file_is_none() {
        assert_eq!(parent_folder("01.flac"), None);
        assert_eq!(
            parent_folder("Lorde/Solar Power (Deluxe Edition)/01.flac"),
            Some("Solar Power (Deluxe Edition)")
        );
        assert_eq!(parent_folder("Lorde/01.flac"), Some("Lorde"));
    }

    #[test]
    fn one_parent_directory_names_the_edition_of_the_whole_album() {
        let parents = BTreeSet::from(["Solar Power (Deluxe Edition)".to_owned()]);
        assert_eq!(
            album_input("Solar Power", &parents, None),
            EditionInput::Labelled("Deluxe Edition".to_owned())
        );
        let parents = BTreeSet::from([
            "Solar Power (Deluxe Edition)/CD1".to_owned(),
            "Solar Power (Deluxe Edition)/CD2".to_owned(),
        ]);
        assert_eq!(
            album_input("Solar Power", &parents, None),
            EditionInput::Absent
        );
        let parents = BTreeSet::from(["CD1".to_owned()]);
        assert_eq!(
            album_input("Solar Power", &parents, None),
            EditionInput::Absent
        );
    }

    #[test]
    fn the_release_comment_wins_over_the_folder() {
        let parents = BTreeSet::from(["Solar Power (Deluxe Edition)".to_owned()]);
        assert_eq!(
            album_input("Solar Power", &parents, Some(" Tour Edition ")),
            EditionInput::Labelled("Tour Edition".to_owned())
        );
        assert_eq!(comment_input(None), EditionInput::Unstated);
        assert_eq!(comment_input(Some("  ")), EditionInput::Unstated);
    }

    #[test]
    fn an_empty_title_states_no_edition() {
        assert_eq!(label_from_folder("", "Solar Power"), None);
        assert_eq!(label_from_folder("...", "Solar Power"), None);
    }
}
