//! Writing a user's saved music into the scanned folder they chose.
//!
//! A save location is per account (`library.save_folder`). An empty value keeps
//! saves in the managed library, which is what every save did before this
//! feature; a value names a scanned folder the account owns, and the music that
//! account saves is written there instead - under `library.naming_template`, as
//! a `folder` media file, with no managed copy kept.
//!
//! Three things read the setting:
//!
//! - the import stage, which resolves the target once per acquisition and writes
//!   a downloaded file straight into the folder ([`SaveTarget`]);
//! - [`SaveFolders::move_saved`], which moves a file the library already holds
//!   into the folder when the user saves or likes the track it belongs to;
//! - [`SaveFolders::reserve`], which resolves the target once for the import and
//!   holds the folder's lock across the file write and the row insert.
//!
//! # An unavailable folder
//!
//! A folder that was removed, unmounted, mounted read-only or reassigned no
//! longer receives saves. The setting is kept, the save goes to the managed
//! library, and the account is told every time with the
//! [`SAVE_FOLDER_UNAVAILABLE`] notice: the situation is self-correcting (the
//! disk can come back), so clearing the setting behind the user's back would
//! lose the answer they gave.
//!
//! # One file per track, and where it comes from
//!
//! A scanned folder has one owner, so a track whose only file is in one account's
//! folder is not moved into another account's: the file is copied, and the first
//! account's file is left alone. A track whose file is in the managed library or
//! cache is moved - one file per track on disk, and the mover is the first saver.
//!
//! # Order of a move, and why
//!
//! A move writes the folder's media row first, then places the file, then deletes
//! the managed row. A crash between the first two steps leaves a folder row that
//! names a missing file, which the next scan prunes while the managed row (and
//! so the playable file) is still there; a crash between the last two leaves both
//! rows, and the next scan prunes the managed one because its file is gone.
//! Either window heals. The other order - place the file, then write the row -
//! would leave a file no row names, which the next scan indexes as a second
//! track, and nothing heals that.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hearthsound_auth::UserSettings;
use hearthsound_catalog::CatalogService;
use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::{Audience, EventBus, ServerEvent, SystemNotice};
use hearthsound_core::providers::CatalogAlbum;
use hearthsound_core::{
    Availability, CatalogId, ClientId, EntityId, EntityKind, IdPrefix, PinReason, StorageClass,
};
use hearthsound_db::Database;
use hearthsound_library::locks::FolderLocks;
use hearthsound_library::naming::{NamingContext, NamingTemplate};
use hearthsound_library::{
    Album, LibraryFolder, LibraryRepository, ReplayGain, ScannedMediaFile, StorageRoots, Track,
    media_files, placement, retention, tracks,
};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::error::AcquireError;

/// The code of the notice a save that could not reach its folder publishes.
pub const SAVE_FOLDER_UNAVAILABLE: &str = "save-folder-unavailable";

/// Where one user's saves are written.
#[derive(Debug, Clone)]
pub enum SaveTarget {
    /// The managed library, `<storage>/library` or `<storage>/cache`.
    Managed,
    /// A scanned folder the saver owns.
    Folder(LibraryFolder),
}

impl SaveTarget {
    /// Returns the folder the target writes into, when it is a scanned folder.
    ///
    /// # Returns
    ///
    /// The folder, or `None` for the managed library.
    pub fn folder(&self) -> Option<&LibraryFolder> {
        match self {
            SaveTarget::Managed => None,
            SaveTarget::Folder(folder) => Some(folder),
        }
    }
}

/// What moving a set of saved items into a folder did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SavedFileReport {
    /// Files that were moved or copied into the folder.
    pub moved: usize,
    /// Tracks whose file was already in the folder.
    pub already_there: usize,
    /// Tracks with no file the move may take (none at all, or only one in a
    /// folder the caller does not own).
    pub no_managed_file: usize,
    /// Tracks whose file could not be placed.
    pub failed: usize,
}

/// A folder an import is writing into, with its lock held.
///
/// The guard serialises the import against a scan of the same folder and against
/// any other save into it, so the file the import places and the row it writes
/// are seen together by both.
pub struct FolderDestination {
    /// The folder the file is written into.
    pub folder: LibraryFolder,
    /// The lock that keeps the scan and the other writers out.
    pub guard: OwnedMutexGuard<()>,
}

impl std::fmt::Debug for FolderDestination {
    /// Reports the type without the guard.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderDestination")
            .field("folder", &self.folder.id)
            .finish_non_exhaustive()
    }
}

/// The save locations of every account, as the acquisition layer resolves them.
pub struct SaveFolders {
    /// The database, for the media rows and the retention rows.
    database: Arc<Database>,
    /// The settings service, for the naming template and the fallback value.
    settings: Settings,
    /// The managed storage roots a managed save lands under.
    roots: StorageRoots,
    /// The per-folder locks, shared with the scanner and the orchestrator.
    locks: Arc<FolderLocks>,
    /// The bus the notice is published on.
    events: Arc<EventBus>,
    /// The library tables.
    library: LibraryRepository,
    /// The catalog service the naming facts read an album's credit from.
    catalog: Arc<CatalogService>,
    /// The per-account setting overrides.
    user_settings: UserSettings,
    /// One lock per track, so two saves of one track never interleave.
    track_locks: Mutex<HashMap<EntityId, Arc<tokio::sync::Mutex<()>>>>,
}

impl std::fmt::Debug for SaveFolders {
    /// Reports the type without its services.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SaveFolders")
    }
}

impl SaveFolders {
    /// Builds the service over the orchestrator's services.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `settings`: the settings service.
    /// - `roots`: the managed storage roots.
    /// - `locks`: the per-folder locks the scanner shares.
    /// - `events`: the bus a save's notice is published on.
    /// - `library`: the library tables.
    /// - `catalog`: the catalog service.
    ///
    /// # Returns
    ///
    /// The service.
    pub fn new(
        database: Arc<Database>,
        settings: Settings,
        roots: StorageRoots,
        locks: Arc<FolderLocks>,
        events: Arc<EventBus>,
        library: LibraryRepository,
        catalog: Arc<CatalogService>,
    ) -> SaveFolders {
        let user_settings = UserSettings::new(&database, settings.registry().clone());
        SaveFolders {
            database,
            settings,
            roots,
            locks,
            events,
            library,
            catalog,
            user_settings,
            track_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Resolves where one user's saves are written.
    ///
    /// The stored value is never trusted: it is re-checked against the folder
    /// table, the ownership and the filesystem on every call, so a folder that
    /// was removed, unmounted or given to another account falls back to the
    /// managed library instead of failing a save. Each fallback publishes the
    /// [`SAVE_FOLDER_UNAVAILABLE`] notice, as the owner decided a save should
    /// warn every time rather than silently change the setting.
    ///
    /// # Parameters
    ///
    /// - `user`: the account saving, or `None` for a server-side save.
    ///
    /// # Returns
    ///
    /// The user's folder when one is configured and usable, and
    /// [`SaveTarget::Managed`] otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Config`] when the setting cannot be read and
    /// [`AcquireError::Library`] when the folder table cannot be read.
    pub async fn target(&self, user: Option<Uuid>) -> Result<SaveTarget, AcquireError> {
        let Some(user) = user else {
            return Ok(SaveTarget::Managed);
        };
        let configured = match self
            .user_settings
            .get_typed(user, keys::LIBRARY_SAVE_FOLDER)
            .await
        {
            Ok(Some(value)) => value,
            Ok(None) => self.settings.current().get(keys::LIBRARY_SAVE_FOLDER)?,
            Err(error) => {
                tracing::warn!(
                    user = %user,
                    %error,
                    "could not read a user's save folder; using the server value"
                );
                self.settings.current().get(keys::LIBRARY_SAVE_FOLDER)?
            }
        };
        if configured.trim().is_empty() {
            return Ok(SaveTarget::Managed);
        }
        let Some(folder_id) = folder_id_of(configured.trim()) else {
            tracing::warn!(
                user = %user,
                value = configured.as_str(),
                "the save folder setting names no folder; keeping the save in the managed library"
            );
            return Ok(SaveTarget::Managed);
        };
        match self.library.owned_folder(user, folder_id).await? {
            Some(folder) if folder.path.is_dir() => Ok(SaveTarget::Folder(folder)),
            Some(folder) => {
                tracing::warn!(
                    user = %user,
                    folder = %folder.path.display(),
                    "the save folder is not there; keeping the save in the managed library"
                );
                self.notice(user, Some(folder.name.as_str()));
                Ok(SaveTarget::Managed)
            }
            None => {
                tracing::warn!(
                    user = %user,
                    folder = %folder_id,
                    "the save folder is not a folder this user owns; keeping the save in the \
                     managed library"
                );
                self.notice(user, None);
                Ok(SaveTarget::Managed)
            }
        }
    }

    /// Resolves the destination and holds its lock for the caller.
    ///
    /// The import uses this once per acquisition: the guard keeps a scan of the
    /// folder and every other writer out until the caller has placed the file
    /// and written its row.
    ///
    /// # Parameters
    ///
    /// - `target`: the target the caller resolved.
    ///
    /// # Returns
    ///
    /// The folder with its lock held, or `None` for a managed target.
    pub async fn reserve(
        &self,
        target: &SaveTarget,
    ) -> Result<Option<FolderDestination>, AcquireError> {
        let Some(folder) = target.folder() else {
            return Ok(None);
        };
        let guard = self.locks.lock(folder.id).await;
        Ok(Some(FolderDestination {
            folder: folder.clone(),
            guard,
        }))
    }

    /// Moves the files of the items a user saved into the user's folder.
    ///
    /// An album is expanded to its tracks; an artist and a playlist hold no file
    /// of their own, so they are ignored. The target is resolved once, so one
    /// unavailable folder produces one notice rather than one per track.
    ///
    /// # Parameters
    ///
    /// - `user`: the account that saved the items.
    /// - `entities`: the saved items with their kinds.
    ///
    /// # Returns
    ///
    /// What the move did, per track.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError`] when the target or an album's tracklist cannot
    /// be read; the moves made before it stand.
    pub async fn move_saved(
        &self,
        user: Uuid,
        entities: &[(EntityKind, EntityId)],
    ) -> Result<SavedFileReport, AcquireError> {
        let mut report = SavedFileReport::default();
        let SaveTarget::Folder(folder) = self.target(Some(user)).await? else {
            return Ok(report);
        };
        let mut seen: HashSet<EntityId> = HashSet::new();
        for (kind, id) in entities {
            let track_ids: Vec<EntityId> = match kind {
                EntityKind::Track => vec![*id],
                EntityKind::Album => self
                    .library
                    .list_tracks_for_album(*id)
                    .await?
                    .into_iter()
                    .map(|track| track.id)
                    .collect(),
                _ => Vec::new(),
            };
            for track_id in track_ids {
                if !seen.insert(track_id) {
                    continue;
                }
                match self.move_one(user, &folder, track_id).await {
                    Ok(MoveOutcome::Placed) => report.moved += 1,
                    Ok(MoveOutcome::AlreadyThere) => report.already_there += 1,
                    Ok(MoveOutcome::NoSourceFile) => report.no_managed_file += 1,
                    Err(error) => {
                        tracing::warn!(
                            user = %user,
                            track = %track_id,
                            folder = %folder.path.display(),
                            %error,
                            "a saved track could not be moved into the save folder"
                        );
                        report.failed += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Moves one track's file into the folder, or copies it when the only file
    /// belongs to another account's folder.
    ///
    /// # Parameters
    ///
    /// - `user`: the account saving the track.
    /// - `folder`: the account's save folder.
    /// - `track_id`: the track to place.
    ///
    /// # Returns
    ///
    /// What happened to the file.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError`] when a row cannot be read or written, and
    /// [`AcquireError::Io`] when the file cannot be placed.
    async fn move_one(
        &self,
        user: Uuid,
        folder: &LibraryFolder,
        track_id: EntityId,
    ) -> Result<MoveOutcome, AcquireError> {
        // The per-track lock first and the folder lock second, in that order
        // everywhere, so two savers of one track and a scan of the folder cannot
        // deadlock each other.
        let track_lock = self.track_lock(track_id);
        let _track_guard = track_lock.lock_owned().await;
        let _folder_guard = self.locks.lock(folder.id).await;

        let Some(track) = self.library.get_track(track_id).await? else {
            return Ok(MoveOutcome::NoSourceFile);
        };
        let files = {
            let mut conn = self.database.readers().acquire().await?;
            media_files::list_media_files_for_track(&mut conn, track_id).await?
        };
        if files
            .iter()
            .any(|file| file.storage == StorageClass::Folder && file.folder_id == Some(folder.id))
        {
            return Ok(MoveOutcome::AlreadyThere);
        }
        let managed = files
            .iter()
            .find(|file| matches!(file.storage, StorageClass::Library | StorageClass::Cache));
        let elsewhere = files
            .iter()
            .find(|file| file.storage == StorageClass::Folder && file.folder_id.is_some());
        let (source, copy, from) = match (managed, elsewhere) {
            (Some(file), _) => (
                self.roots.root_for(file.storage).join(&file.rel_path),
                false,
                file,
            ),
            (None, Some(file)) => {
                let from = match file.folder_id {
                    Some(folder_id) => self.library.get_library_folder(folder_id).await?,
                    None => None,
                };
                let Some(from) = from else {
                    return Ok(MoveOutcome::NoSourceFile);
                };
                (from.path.join(&file.rel_path), true, file)
            }
            (None, None) => return Ok(MoveOutcome::NoSourceFile),
        };

        let snapshot = self.settings.current();
        let template = NamingTemplate::from_settings(&snapshot)?;
        let album = match track.album_id {
            Some(album_id) => self.library.get_album(album_id).await?,
            None => None,
        };
        let facts = load_naming_facts(&self.library, &self.catalog, &track, album).await?;
        let relative = match Path::new(&from.rel_path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
        {
            Some(extension) => facts.render(&template, &extension)?,
            // A row whose path names no container has none to render, so its
            // stored relative path is the best name there is.
            None => PathBuf::from(&from.rel_path),
        };
        let destination =
            placement::free_destination(&folder.path.join(&relative), &HashSet::new())
                .await
                .map_err(AcquireError::Library)?;
        let stored = destination
            .strip_prefix(&folder.path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| relative.to_string_lossy().into_owned());
        let scan = ScannedMediaFile {
            folder_id: folder.id,
            rel_path: stored,
            size: from.size,
            mtime_ms: from.mtime_ms,
            format: from.format.clone(),
            codec: from.codec.clone(),
            bitrate_kbps: from.bitrate_kbps,
            sample_rate: from.sample_rate,
            bit_depth: from.bit_depth,
            channels: from.channels,
            duration_ms: from.duration_ms,
            lossless: from.lossless,
            replaygain: from.replaygain.as_deref().and_then(ReplayGain::from_json),
            has_embedded_art: from.has_embedded_art,
        };

        // The row goes in first: a crash from here to the move leaves a row
        // naming a missing file, which the next scan prunes while the file that
        // is still in managed storage stays playable.
        let mut tx = self.database.writer().begin().await?;
        let folder_file = media_files::upsert_folder_media_file(&mut tx, track_id, &scan).await?;
        retention::add_pin_reason(&mut tx, track_id, PinReason::Folder).await?;
        tracks::set_track_availability(&mut tx, track_id, Availability::Local, None).await?;
        tx.commit().await?;

        let placed = if copy {
            placement::copy_file_durable(&source, &destination).await
        } else {
            placement::move_file_unshared(&source, &destination).await
        };
        if let Err(error) = placed {
            let mut tx = self.database.writer().begin().await?;
            if let Err(database) = media_files::delete_media_file(&mut tx, folder_file).await {
                tracing::warn!(
                    user = %user,
                    track = %track_id,
                    error = %database,
                    "the folder row a failed save wrote could not be removed"
                );
            }
            tx.commit().await?;
            self.notice(user, Some(folder.name.as_str()));
            return Err(error.into());
        }

        if !copy {
            let cover_filename = snapshot.get(keys::LIBRARY_COVER_FILENAME)?;
            self.copy_cover(&source, &destination, &cover_filename)
                .await;
            let mut tx = self.database.writer().begin().await?;
            let deleted = media_files::delete_media_file(&mut tx, from.id).await?;
            tx.commit().await?;
            if !deleted {
                tracing::debug!(
                    user = %user,
                    track = %track_id,
                    media_file = %from.id,
                    "the managed row of a moved file was already gone"
                );
            }
            if let Err(error) =
                placement::prune_empty_parents(&source, self.roots.root_for(from.storage)).await
            {
                tracing::debug!(
                    path = %source.display(),
                    %error,
                    "the directories a move emptied could not be pruned"
                );
            }
        }
        Ok(MoveOutcome::Placed)
    }

    /// Copies a managed file's sidecar cover beside its new path.
    ///
    /// The managed album directory keeps its own cover, because the other tracks
    /// of the album are still there and their cover belongs to all of them; the
    /// copy is skipped when the destination directory already has one.
    ///
    /// # Parameters
    ///
    /// - `source`: the file that moved.
    /// - `destination`: the path it moved to.
    /// - `cover_filename`: the sidecar file name in effect.
    async fn copy_cover(&self, source: &Path, destination: &Path, cover_filename: &str) {
        let (Some(from_dir), Some(to_dir)) = (source.parent(), destination.parent()) else {
            return;
        };
        let from = from_dir.join(cover_filename);
        let to = to_dir.join(cover_filename);
        match placement::path_exists(&to).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(
                    path = %to.display(),
                    %error,
                    "the cover of a moved file could not be checked"
                );
                return;
            }
        }
        if let Err(error) = placement::copy_file_durable(&from, &to).await {
            tracing::debug!(
                from = %from.display(),
                to = %to.display(),
                %error,
                "the cover of a moved file could not be copied"
            );
        }
    }

    /// Returns the lock of one track and builds it when it is missing.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track's id.
    ///
    /// # Returns
    ///
    /// The track's lock.
    fn track_lock(&self, track_id: EntityId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .track_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(track_id).or_default())
    }

    /// Tells a user that their save could not be written into their folder.
    ///
    /// The notice reaches the account and its administrators, which is the
    /// audience rule the event bus applies to [`Audience::User`].
    ///
    /// # Parameters
    ///
    /// - `user`: the account that saved.
    /// - `folder_name`: the folder's display name, when it is known.
    pub fn notice(&self, user: Uuid, folder_name: Option<&str>) {
        let message = match folder_name {
            Some(name) => format!(
                "Saved music could not be written to \"{name}\"; it was kept in the managed \
                 library instead."
            ),
            None => "Saved music could not be written to your save folder; it was kept in the \
                     managed library instead."
                .to_owned(),
        };
        self.events.publish_for(
            Audience::User(user),
            ServerEvent::SystemNotice(SystemNotice {
                level: "warn".to_owned(),
                message,
                code: Some(SAVE_FOLDER_UNAVAILABLE.to_owned()),
            }),
        );
    }
}

/// What [`SaveFolders::move_one`] did with one track.
enum MoveOutcome {
    /// The file is in the folder now.
    Placed,
    /// The track already had a file in the folder.
    AlreadyThere,
    /// There was no file the move or the copy could take.
    NoSourceFile,
}

/// Parses a `library.save_folder` value into the folder id it names.
///
/// # Parameters
///
/// - `value`: the stored value: the client id of a scanned folder, or a bare
///   row id as an operator's configuration file may carry it.
///
/// # Returns
///
/// The folder's row id, or `None` when the value names no folder.
fn folder_id_of(value: &str) -> Option<Uuid> {
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::LibraryFolder => Some(id.uuid()),
        _ => Uuid::parse_str(value).ok(),
    }
}

/// The facts the naming template renders an imported or saved file from.
///
/// Both the import and the move render the destination from this, so a file the
/// server writes and a file it later moves into a folder are named from the same
/// inputs: the album artist the sources agree on, the album's title and year, the
/// disc and track numbers, and the track's title.
#[derive(Debug, Clone)]
pub(crate) struct NamingFacts {
    /// The album artist the file is named and tagged with.
    pub(crate) album_artist: Option<String>,
    /// The album's title.
    pub(crate) album: Option<String>,
    /// The album's release year.
    pub(crate) year: Option<i64>,
    /// The track's disc number.
    pub(crate) disc_number: Option<u32>,
    /// How many discs the release has.
    pub(crate) disc_total: Option<u32>,
    /// The track's number on its disc.
    pub(crate) track_number: Option<u32>,
    /// How many tracks the track's disc holds.
    pub(crate) track_total: Option<u32>,
    /// The track's title.
    pub(crate) title: String,
}

impl NamingFacts {
    /// Renders the destination a file is imported or moved to.
    ///
    /// # Parameters
    ///
    /// - `template`: the naming template in effect.
    /// - `extension`: the file's container, without the dot.
    ///
    /// # Returns
    ///
    /// The path relative to the storage root or the folder root.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Naming`] when the template renders no usable
    /// path.
    pub(crate) fn render(
        &self,
        template: &NamingTemplate,
        extension: &str,
    ) -> Result<PathBuf, AcquireError> {
        let context = NamingContext {
            album_artist: self.album_artist.as_deref(),
            album: self.album.as_deref(),
            year: self.year,
            disc_number: self.disc_number,
            disc_total: self.disc_total,
            track_number: self.track_number,
            title: Some(self.title.as_str()),
            extension: Some(extension),
        };
        Ok(template.render(&context)?)
    }
}

/// The already-read rows the naming facts are picked from.
///
/// The import reads every one of these for its tags anyway, so it hands them in
/// rather than paying for a second read; anything that does not have them reads
/// them through [`load_naming_facts`]. Both go through [`naming_facts`], which is
/// what keeps a file the server writes and a file it later moves into a folder
/// named the same way.
pub(crate) struct NamingInputs<'a> {
    /// The track's album row, when it has one.
    pub(crate) album: Option<&'a Album>,
    /// The album's stored track rows.
    pub(crate) album_tracks: &'a [Track],
    /// What the album's issuing catalog instance contributes.
    pub(crate) issued: &'a IssuedAlbum,
    /// The artists the track is credited to.
    pub(crate) artists: &'a [hearthsound_library::Artist],
    /// The artists the album is credited to.
    pub(crate) album_artists: &'a [hearthsound_library::Artist],
}

/// Reads everything the naming template needs for one track.
///
/// This is the form for a caller that has not read the track's album and credits
/// yet, which is the move path: it pays for the reads and then calls
/// [`naming_facts`].
///
/// # Parameters
///
/// - `library`: the library tables.
/// - `catalog`: the catalog service.
/// - `track`: the track.
/// - `album`: the track's album, when it has one.
///
/// # Returns
///
/// The facts the template renders from.
///
/// # Errors
///
/// Returns [`AcquireError::Library`] when one of the album or artist reads
/// fails; the catalog read degrades to [`IssuedAlbum::Unavailable`] instead, so a
/// save never fails on a provider.
pub(crate) async fn load_naming_facts(
    library: &LibraryRepository,
    catalog: &CatalogService,
    track: &Track,
    album: Option<Album>,
) -> Result<NamingFacts, AcquireError> {
    let album_tracks = match track.album_id {
        Some(album_id) => library.list_tracks_for_album(album_id).await?,
        None => Vec::new(),
    };
    let issued = match track.album_id {
        Some(album_id) => issued_album(catalog, album_id, track.disc_number).await,
        None => IssuedAlbum::Local,
    };
    let artists = library.list_track_artists(track.id).await?;
    let album_artists = match track.album_id {
        Some(album_id) => library.list_album_artists(album_id).await?,
        None => Vec::new(),
    };
    Ok(naming_facts(
        track,
        NamingInputs {
            album: album.as_ref(),
            album_tracks: &album_tracks,
            issued: &issued,
            artists: &artists,
            album_artists: &album_artists,
        },
    ))
}

/// Picks the facts the naming template renders one track from.
///
/// The album artist comes from the sources the import already trusts: the stored
/// album row, the album's first credit, the issuing catalog instance's credit,
/// and finally the track's own artist. The disc and track totals come from the
/// issuing catalog instance's album when one issued the album, and from the
/// stored rows otherwise.
///
/// # Parameters
///
/// - `track`: the track the file is for.
/// - `inputs`: the rows the facts are picked from.
///
/// # Returns
///
/// The facts the template renders from.
pub(crate) fn naming_facts(track: &Track, inputs: NamingInputs<'_>) -> NamingFacts {
    let album_artist = AlbumArtistSources {
        stored: inputs
            .album
            .and_then(|album| album.display_artist.as_deref()),
        stored_credit: inputs
            .album_artists
            .first()
            .map(|artist| artist.name.as_str()),
        catalog_credit: inputs.issued.album_artist(),
        track: track.display_artist.as_deref(),
        track_credit: inputs.artists.first().map(|artist| artist.name.as_str()),
    }
    .pick();
    let AlbumTotals {
        disc_total,
        track_total,
    } = match inputs.issued {
        IssuedAlbum::Catalog { totals, .. } => *totals,
        IssuedAlbum::Local => totals_of_stored_rows(inputs.album_tracks),
        IssuedAlbum::Unavailable => AlbumTotals::default(),
    };
    NamingFacts {
        album_artist,
        album: inputs.album.map(|album| album.title.clone()),
        year: inputs.album.and_then(|album| album.year),
        disc_number: track.disc_number.and_then(|disc| u32::try_from(disc).ok()),
        disc_total,
        track_number: track
            .track_number
            .and_then(|number| u32::try_from(number).ok()),
        track_total,
        title: track.title.clone(),
    }
}

/// What an album row's issuing catalog instance contributes to a name.
///
/// A materialized catalog track can be the only stored row of its album, so the
/// album row says nothing about how many tracks or discs the release has; the
/// album its issuing instance lists is then the source. An album nothing issued
/// is local, and its stored rows are all of it.
pub(crate) enum IssuedAlbum {
    /// The album the issuing catalog instance lists: the artist it credits the
    /// album with, when it credits one, and the totals its tracklist implies for
    /// the imported track.
    Catalog {
        /// The first artist the album credits.
        album_artist: Option<String>,
        /// The disc and track totals the tracklist implies.
        totals: AlbumTotals,
    },
    /// No catalog instance issued the album, so its stored rows are all of it.
    Local,
    /// A catalog instance issued the album but its album could not be read.
    Unavailable,
}

impl IssuedAlbum {
    /// Returns the artist the issuing catalog instance credits the album with.
    ///
    /// # Returns
    ///
    /// The credit's display name, or `None` for anything but a catalog album
    /// that credits an artist. A provider whose track-embedded album carries no
    /// artist - Deezer's, for example - leaves the caller to fall back to the
    /// track's own artist, which is the only album artist such an album has.
    fn album_artist(&self) -> Option<&str> {
        match self {
            IssuedAlbum::Catalog { album_artist, .. } => album_artist.as_deref(),
            IssuedAlbum::Local | IssuedAlbum::Unavailable => None,
        }
    }
}

/// Reads the album an album row's issuing catalog instance lists.
///
/// The album is found through the `p:<slug>` alias the materialization wrote, and
/// read through the catalog cache, so a listing an album page just fetched costs
/// no second provider call.
///
/// # Parameters
///
/// - `catalog`: the catalog service.
/// - `album_id`: the stored album row to resolve.
/// - `disc_number`: the track's disc number, which selects the disc a multi-disc
///   album's track total counts.
///
/// # Returns
///
/// [`IssuedAlbum::Catalog`] for an album a catalog instance issued,
/// [`IssuedAlbum::Local`] for one nothing issued, and
/// [`IssuedAlbum::Unavailable`] when the alias exists but its album could not be
/// read. A failure is logged at debug and never propagates: a save that cannot
/// reach its catalog instance still writes the file.
pub(crate) async fn issued_album(
    catalog: &CatalogService,
    album_id: EntityId,
    disc_number: Option<i64>,
) -> IssuedAlbum {
    let alias = match catalog.provider_aliases(album_id).await {
        Ok(aliases) => match aliases.into_iter().next() {
            Some(alias) => alias,
            None => return IssuedAlbum::Local,
        },
        Err(error) => {
            tracing::debug!(
                album = %album_id,
                %error,
                "the provider alias of an album could not be read"
            );
            return IssuedAlbum::Unavailable;
        }
    };
    let id = match CatalogId::new(alias.slug, EntityKind::Album, alias.provider_id) {
        Ok(id) => id,
        Err(error) => {
            tracing::debug!(
                album = %album_id,
                %error,
                "the provider alias of an album does not name an album"
            );
            return IssuedAlbum::Unavailable;
        }
    };
    match catalog.album(&id).await {
        Ok(result) => IssuedAlbum::Catalog {
            album_artist: result
                .album
                .artists
                .first()
                .map(|credit| credit.name.clone()),
            totals: totals_of_catalog_album(&result.album, disc_number),
        },
        Err(error) => {
            tracing::debug!(
                album = %album_id,
                instance = id.slug().as_str(),
                %error,
                "the catalog album of an album row could not be read"
            );
            IssuedAlbum::Unavailable
        }
    }
}

/// The sources the album artist is picked from, in order of reliability.
///
/// The stored album row comes first because it is what the rest of the library
/// shows. The issuing catalog instance's own album comes next, because the album
/// reference a lone materialized track carries may omit the artist - Deezer's
/// does - while its full album lists one. The track's own artist is the last
/// resort: an album nobody credited still has to produce a file whose path and
/// `albumartist` tag name an artist.
struct AlbumArtistSources<'a> {
    /// The album row's display artist.
    stored: Option<&'a str>,
    /// The first artist the album stored a credit for.
    stored_credit: Option<&'a str>,
    /// The first artist the issuing catalog instance credits the album with.
    catalog_credit: Option<&'a str>,
    /// The track's display artist.
    track: Option<&'a str>,
    /// The first artist the track is credited to.
    track_credit: Option<&'a str>,
}

impl AlbumArtistSources<'_> {
    /// Returns the album artist the file is named and tagged with.
    ///
    /// # Returns
    ///
    /// The first source that names an artist, or `None` when none of them does.
    fn pick(&self) -> Option<String> {
        self.stored
            .or(self.stored_credit)
            .or(self.catalog_credit)
            .or(self.track)
            .or(self.track_credit)
            .map(str::to_owned)
    }
}

/// The disc and track totals a file is tagged and rendered with.
///
/// A total the sources cannot derive is `None`, which writes no tag for it and
/// renders no disc prefix rather than claiming a number nothing supports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AlbumTotals {
    /// How many discs the release has, when it is known to have more than one.
    pub(crate) disc_total: Option<u32>,
    /// How many tracks the track's disc holds, when it is known.
    pub(crate) track_total: Option<u32>,
}

/// Counts the totals a catalog album's tracklist implies.
///
/// A multi-disc release counts only the tracks it holds on the imported track's
/// own disc, which is what a `TRACKTOTAL` tag on such a release means, and a
/// multi-disc release whose imported track names no disc gets no track total,
/// because the disc the count would describe is unknown.
///
/// # Parameters
///
/// - `album`: the album the issuing instance lists.
/// - `disc_number`: the imported track's disc number.
///
/// # Returns
///
/// The release's disc count and the imported disc's track count.
pub(crate) fn totals_of_catalog_album(
    album: &CatalogAlbum,
    disc_number: Option<i64>,
) -> AlbumTotals {
    let disc_total = album
        .tracks
        .iter()
        .filter_map(|track| track.disc_number)
        .max()
        .filter(|disc| *disc > 1)
        .map(u32::from);
    let track_total = match disc_total {
        Some(_) => disc_number
            .and_then(|disc| u16::try_from(disc).ok())
            .and_then(|disc| {
                let count = album
                    .tracks
                    .iter()
                    .filter(|track| track.disc_number == Some(disc))
                    .count();
                u32::try_from(count).ok()
            }),
        None => u32::try_from(album.tracks.len()).ok(),
    }
    .filter(|count| *count > 0);
    AlbumTotals {
        disc_total,
        track_total,
    }
}

/// Counts the totals a local album's stored rows imply.
///
/// # Parameters
///
/// - `rows`: the album's stored track rows.
///
/// # Returns
///
/// The highest disc number the rows carry, when they carry more than one, and
/// how many rows the album has.
pub(crate) fn totals_of_stored_rows(rows: &[Track]) -> AlbumTotals {
    let disc_total = rows
        .iter()
        .filter_map(|track| track.disc_number)
        .max()
        .and_then(|disc| u32::try_from(disc).ok())
        .filter(|disc| *disc > 1);
    let track_total = u32::try_from(rows.len()).ok().filter(|count| *count > 0);
    AlbumTotals {
        disc_total,
        track_total,
    }
}

#[cfg(test)]
mod tests {
    use hearthsound_core::IdPrefix;
    use hearthsound_core::providers::CatalogTrack;
    use hearthsound_library::Origin;
    use uuid::Uuid;

    use super::*;

    /// Builds a catalog album holding `discs[i]` tracks on disc `i + 1`.
    ///
    /// # Parameters
    ///
    /// - `discs`: the track count of each disc, disc 1 first.
    fn album(discs: &[u16]) -> CatalogAlbum {
        let mut tracks = Vec::new();
        for (index, count) in discs.iter().enumerate() {
            let disc = u16::try_from(index + 1).unwrap();
            for number in 1..=*count {
                tracks.push(CatalogTrack {
                    id: format!("track-{disc}-{number}"),
                    title: format!("Song {disc}-{number}"),
                    artists: Vec::new(),
                    album: None,
                    duration_ms: None,
                    disc_number: Some(disc),
                    track_number: Some(number),
                    isrc: None,
                    explicit: None,
                    ids: Vec::new(),
                });
            }
        }
        CatalogAlbum {
            id: "album-1".to_owned(),
            title: "Album".to_owned(),
            artists: Vec::new(),
            release_date: None,
            album_type: None,
            upc: None,
            genres: Vec::new(),
            images: Vec::new(),
            tracks,
            ids: Vec::new(),
        }
    }

    /// Builds a stored track row on the given disc.
    ///
    /// # Parameters
    ///
    /// - `disc_number`: the row's disc number.
    fn row(disc_number: Option<i64>) -> Track {
        Track {
            id: EntityId::from_uuid(IdPrefix::Track, Uuid::nil()),
            album_id: None,
            title: "Song".to_owned(),
            sort_title: "Song".to_owned(),
            display_artist: None,
            disc_number,
            track_number: None,
            duration_ms: None,
            isrc: None,
            mb_recording_id: None,
            explicit: None,
            origin: Origin::Catalog,
            availability: Availability::Remote,
            unavailable_until: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn the_album_artist_comes_from_the_first_source_that_names_one() {
        let all = AlbumArtistSources {
            stored: Some("Stored"),
            stored_credit: Some("Stored Credit"),
            catalog_credit: Some("Catalog Credit"),
            track: Some("Track Artist"),
            track_credit: Some("Track Credit"),
        };
        assert_eq!(all.pick().as_deref(), Some("Stored"));

        let without_stored = AlbumArtistSources {
            stored: None,
            ..all
        };
        assert_eq!(without_stored.pick().as_deref(), Some("Stored Credit"));

        let catalog_only = AlbumArtistSources {
            stored_credit: None,
            ..without_stored
        };
        assert_eq!(catalog_only.pick().as_deref(), Some("Catalog Credit"));

        let track_only = AlbumArtistSources {
            catalog_credit: None,
            ..catalog_only
        };
        assert_eq!(track_only.pick().as_deref(), Some("Track Artist"));

        let credit_only = AlbumArtistSources {
            track: None,
            ..track_only
        };
        assert_eq!(credit_only.pick().as_deref(), Some("Track Credit"));

        let nothing = AlbumArtistSources {
            track_credit: None,
            ..credit_only
        };
        assert_eq!(nothing.pick(), None);
    }

    #[test]
    fn a_single_disc_album_counts_all_of_its_tracks() {
        assert_eq!(
            totals_of_catalog_album(&album(&[5]), Some(1)),
            AlbumTotals {
                disc_total: None,
                track_total: Some(5),
            }
        );
    }

    #[test]
    fn a_multi_disc_album_counts_the_tracks_of_the_imported_disc() {
        let album = album(&[4, 3]);
        assert_eq!(
            totals_of_catalog_album(&album, Some(2)),
            AlbumTotals {
                disc_total: Some(2),
                track_total: Some(3),
            }
        );
        assert_eq!(
            totals_of_catalog_album(&album, Some(1)),
            AlbumTotals {
                disc_total: Some(2),
                track_total: Some(4),
            }
        );
    }

    #[test]
    fn a_multi_disc_album_without_a_disc_number_writes_no_track_total() {
        assert_eq!(
            totals_of_catalog_album(&album(&[4, 3]), None),
            AlbumTotals {
                disc_total: Some(2),
                track_total: None,
            }
        );
    }

    #[test]
    fn an_album_without_tracks_writes_no_total() {
        assert_eq!(
            totals_of_catalog_album(&album(&[]), Some(1)),
            AlbumTotals::default()
        );
        assert_eq!(totals_of_stored_rows(&[]), AlbumTotals::default());
    }

    #[test]
    fn stored_rows_count_toward_a_local_album() {
        assert_eq!(
            totals_of_stored_rows(&[row(Some(1)), row(Some(1)), row(Some(2))]),
            AlbumTotals {
                disc_total: Some(2),
                track_total: Some(3),
            }
        );
        assert_eq!(
            totals_of_stored_rows(&[row(Some(1)), row(None)]),
            AlbumTotals {
                disc_total: None,
                track_total: Some(2),
            }
        );
    }
}
