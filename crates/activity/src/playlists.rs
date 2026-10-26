//! Playlists, their entries, and the playlist pin those entries imply.
//!
//! A playlist belongs to one owner and holds track ids in order. Every edit
//! renumbers the positions from zero, so a position is always the entry's index
//! in the playlist, and the pin of every track whose membership changed is
//! refreshed in the same transaction: a track carries the `playlist` pin while
//! it is in any non-temporary playlist.
//!
//! A playlist is permanent unless it is made temporary. A temporary playlist is
//! one the server produced for a bounded purpose - a synced playlist, for
//! example - and it pins none of the tracks it holds, so those tracks stay
//! evictable until [`Playlists::set_lifetime`] makes the playlist permanent. A
//! temporary playlist may carry an expiry, and [`Playlists::delete_expired`],
//! which the `playlists.expire` job runs, deletes the ones whose expiry has
//! passed.

use async_trait::async_trait;
use hearthsound_core::{EntityId, IdPrefix, PinReason};
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler};
use hearthsound_library::maintenance::MaintenanceSignal;
use hearthsound_library::retention;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::error::ActivityError;
use crate::now_millis;

/// How long a playlist name may be.
pub const MAX_PLAYLIST_NAME: usize = 255;

/// A playlist and its entries.
///
/// # Examples
///
/// ```no_run
/// # async fn example(db: &hearthsound_db::Database) {
/// use hearthsound_activity::Playlists;
///
/// let playlists = Playlists::new(db);
/// # let _ = playlists;
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    /// The playlist's client id.
    pub id: EntityId,
    /// The account that owns it.
    pub owner_user_id: Uuid,
    /// Its name.
    pub name: String,
    /// Its comment, when it has one.
    pub comment: Option<String>,
    /// Whether every user may see it.
    pub is_public: bool,
    /// Whether it is temporary.
    ///
    /// A temporary playlist holds tracks the server put there - a synced
    /// playlist, for example - and pins none of them, so its tracks are
    /// evictable until the playlist is made permanent.
    pub is_temporary: bool,
    /// When it expires, in unix milliseconds.
    ///
    /// Only a temporary playlist carries an expiry, and a temporary playlist
    /// without one never expires by itself. [`Playlists::delete_expired`]
    /// removes the temporary playlists whose expiry has passed.
    pub expires_at: Option<i64>,
    /// The instance a generated playlist came from.
    ///
    /// It is set when the playlist was imported from a discovery service and
    /// cleared when that instance is removed.
    pub source_instance_id: Option<Uuid>,
    /// The service's stable reference for a generated playlist.
    ///
    /// The pair of this reference and [`Playlist::source_instance_id`] is what
    /// a refresh matches, so a second sync replaces the playlist in place.
    pub source_ref: Option<String>,
    /// The display name of the instance a generated playlist came from.
    ///
    /// It is read with the playlist so a client can say which service generated
    /// it without a second lookup; it is `None` for a playlist no service
    /// generated and for one whose instance has been removed.
    pub source_instance_name: Option<String>,
    /// When it was created, in unix milliseconds.
    pub created_at: i64,
    /// When it last changed, in unix milliseconds.
    pub updated_at: i64,
    /// Its entries, in playing order.
    pub entries: Vec<PlaylistEntry>,
}

impl Playlist {
    /// Returns the tracks of the playlist, in playing order.
    pub fn track_ids(&self) -> Vec<EntityId> {
        self.entries.iter().map(|entry| entry.track_id).collect()
    }
}

/// One entry of a playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaylistEntry {
    /// The track the entry holds.
    pub track_id: EntityId,
    /// The entry's zero-based position in the playlist.
    pub position: i64,
    /// When the entry was added, in unix milliseconds.
    pub added_at: i64,
}

/// The changes one edit applies to a playlist.
///
/// A field left at `None` is not changed, so an edit that only adds tracks
/// leaves the name alone.
///
/// # Examples
///
/// ```
/// use hearthsound_activity::PlaylistUpdate;
///
/// let edit = PlaylistUpdate {
///     name: Some("Evening".to_owned()),
///     ..PlaylistUpdate::default()
/// };
/// assert!(edit.add.is_empty());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistUpdate {
    /// A new name.
    pub name: Option<String>,
    /// A new comment; an empty string clears it.
    pub comment: Option<String>,
    /// A new visibility.
    pub public: Option<bool>,
    /// Tracks to append, in order.
    pub add: Vec<EntityId>,
    /// Positions to remove, in any order. Indexes are read against the entries
    /// the playlist has before the additions, which is the order a client saw
    /// them in.
    pub remove_indexes: Vec<usize>,
}

/// One generated playlist the sync job writes.
///
/// The owner, the generating instance and the service's reference identify the
/// playlist, so writing the same slot twice replaces its name, description,
/// expiry and tracks instead of creating a second playlist.
///
/// # Examples
///
/// ```
/// use hearthsound_activity::SyncedPlaylist;
/// use uuid::Uuid;
///
/// let write = SyncedPlaylist {
///     owner_user_id: Uuid::nil(),
///     source_instance_id: Uuid::nil(),
///     source_ref: "weekly-jams",
///     name: "Weekly Jams",
///     comment: None,
///     expires_at: 4_000,
///     tracks: &[],
/// };
/// assert_eq!(write.name, "Weekly Jams");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncedPlaylist<'a> {
    /// The account the playlist belongs to.
    pub owner_user_id: Uuid,
    /// The discovery instance that generated it.
    pub source_instance_id: Uuid,
    /// The service's stable reference for the playlist slot.
    pub source_ref: &'a str,
    /// The name the service gave it.
    pub name: &'a str,
    /// The description the service gave it, when it gave one.
    pub comment: Option<&'a str>,
    /// When the playlist expires, in unix milliseconds.
    pub expires_at: i64,
    /// The tracks it holds afterwards, in order.
    pub tracks: &'a [EntityId],
}

/// One stored entry of a playlist.
#[derive(sqlx::FromRow)]
struct EntryRow {
    /// The stored position.
    position: i64,
    /// The stored track id.
    track_id: Vec<u8>,
    /// When the entry was added.
    added_at: i64,
}

/// One stored playlist row.
#[derive(sqlx::FromRow)]
struct PlaylistRow {
    /// The stored id.
    id: Vec<u8>,
    /// The owner.
    owner_user_id: Vec<u8>,
    /// The name.
    name: String,
    /// The comment.
    comment: Option<String>,
    /// The visibility flag.
    is_public: i64,
    /// The temporary flag.
    is_temporary: i64,
    /// The expiry instant.
    expires_at: Option<i64>,
    /// The generating instance, when the playlist came from one.
    source_instance_id: Option<Vec<u8>>,
    /// The service's reference for the generated playlist.
    source_ref: Option<String>,
    /// When the listener first edited the generated playlist.
    detached_at: Option<i64>,
    /// The display name of the generating instance.
    source_instance_name: Option<String>,
    /// The creation instant.
    created_at: i64,
    /// The last-change instant.
    updated_at: i64,
}

/// The `playlists` and `playlist_tracks` tables.
///
/// Cloning is cheap: every clone shares the same two pools.
#[derive(Debug, Clone)]
pub struct Playlists {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
    /// Woken after an edit, because an edit changes `playlist` pins.
    signal: MaintenanceSignal,
}

impl Playlists {
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
    pub fn new(db: &Database) -> Playlists {
        Playlists {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            signal: MaintenanceSignal::new(),
        }
    }

    /// Wakes the given storage maintenance signal after every edit.
    ///
    /// # Parameters
    ///
    /// - `signal`: the signal the maintenance worker waits on.
    ///
    /// # Returns
    ///
    /// The repository, notifying that signal.
    pub fn with_signal(mut self, signal: MaintenanceSignal) -> Playlists {
        self.signal = signal;
        self
    }

    /// Creates a playlist.
    ///
    /// # Parameters
    ///
    /// - `owner`: the account the playlist belongs to.
    /// - `name`: the playlist name.
    /// - `comment`: the playlist comment.
    /// - `public`: whether every user may see the playlist.
    /// - `tracks`: the tracks to add, in order.
    ///
    /// # Returns
    ///
    /// The created playlist, with its entries.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::InvalidName`] when the name is empty,
    /// [`ActivityError::Database`] when a row cannot be written, and
    /// [`ActivityError::Library`] when a pin cannot be updated.
    pub async fn create(
        &self,
        owner: Uuid,
        name: &str,
        comment: Option<&str>,
        public: bool,
        tracks: &[EntityId],
    ) -> Result<Playlist, ActivityError> {
        let name = checked_name(name)?;
        let now = now_millis();
        let id = EntityId::new(IdPrefix::Playlist);
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        sqlx::query!(
            "INSERT INTO playlists (id, owner_user_id, name, comment, is_public, is_temporary, \
                                    created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
            entity_key(id),
            owner.as_bytes().as_slice(),
            name,
            comment,
            public,
            now,
            now,
        )
        .execute(&mut *tx)
        .await?;
        let entries = replace_entries(&mut tx, id, tracks, now).await?;
        for track in tracks {
            refresh_playlist_pin(&mut tx, *track).await?;
        }
        tx.commit().await?;
        self.signal.notify();
        Ok(Playlist {
            id,
            owner_user_id: owner,
            name: name.to_owned(),
            comment: comment.map(str::to_owned),
            is_public: public,
            is_temporary: false,
            expires_at: None,
            source_instance_id: None,
            source_ref: None,
            source_instance_name: None,
            created_at: now,
            updated_at: now,
            entries,
        })
    }

    /// Writes one generated playlist, replacing the slot it names.
    ///
    /// The playlist is identified by its owner, the generating instance and the
    /// service's reference, so a second sync of the same slot keeps the
    /// playlist's id and rewrites its name, description, expiry and tracks.
    /// A new slot is created temporary, which is what a generated playlist is:
    /// it pins none of its tracks, so they stay evictable until a listener
    /// makes the playlist permanent.
    ///
    /// A slot the listener has made permanent keeps that lifetime across a
    /// sync: its expiry stays cleared, so the expiry sweep never deletes a
    /// playlist the listener chose to keep.
    ///
    /// A slot the listener has edited is never refreshed again. Its row keeps
    /// the owner, instance and reference that identify the slot, so the sync
    /// recognises it instead of creating a copy, and answers `None`: nothing is
    /// written, nothing is published, and the playlist stays exactly as the
    /// listener left it.
    ///
    /// The name, the expiry and every entry are written in one transaction, so
    /// a reader never sees a playlist that is half refreshed.
    ///
    /// # Parameters
    ///
    /// - `write`: the playlist slots and the tracks it holds afterwards.
    /// - `now`: the instant of the sync, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// The playlist as it was written, with its entries, or `None` when the
    /// slot belongs to a playlist the listener edited.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::InvalidName`] when the name is empty or too
    /// long, and [`ActivityError::Database`] or [`ActivityError::Library`] when
    /// a read or write fails.
    pub async fn upsert_generated(
        &self,
        write: &SyncedPlaylist<'_>,
        now: i64,
    ) -> Result<Option<Playlist>, ActivityError> {
        let name = checked_name(write.name)?;
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        let found = sqlx::query!(
            "SELECT id, detached_at FROM playlists \
             WHERE owner_user_id = ? AND source_instance_id = ? AND source_ref = ?",
            write.owner_user_id.as_bytes().as_slice(),
            write.source_instance_id.as_bytes().as_slice(),
            write.source_ref,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if found.as_ref().is_some_and(|row| row.detached_at.is_some()) {
            // The listener made this playlist their own; the slot is remembered
            // only so a second copy is never created.
            return Ok(None);
        }
        let existing = found.as_ref().map(|row| row.id.clone());
        let id = match &existing {
            Some(id) => EntityId::from_uuid(IdPrefix::Playlist, Uuid::from_slice(id)?),
            None => EntityId::new(IdPrefix::Playlist),
        };
        let before = match existing {
            Some(_) => Some(load_or_unknown(&mut tx, id).await?),
            None => None,
        };
        let affected = before
            .as_ref()
            .map(|playlist| affected_tracks(&playlist.entries, write.tracks))
            .unwrap_or_default();
        // A slot the listener made permanent stays permanent: the sync rewrites
        // what the service generated, not whether the listener keeps it.
        let (temporary, expires_at) = match &before {
            Some(playlist) if !playlist.is_temporary => (false, None),
            _ => (true, Some(write.expires_at)),
        };
        match &before {
            Some(_) => {
                sqlx::query!(
                    "UPDATE playlists \
                     SET name = ?, comment = ?, is_temporary = ?, expires_at = ?, updated_at = ? \
                     WHERE id = ?",
                    name,
                    write.comment,
                    temporary,
                    expires_at,
                    now,
                    entity_key(id),
                )
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query!(
                    "INSERT INTO playlists (id, owner_user_id, name, comment, is_public, \
                                            is_temporary, expires_at, source_instance_id, \
                                            source_ref, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, 0, 1, ?, ?, ?, ?, ?)",
                    entity_key(id),
                    write.owner_user_id.as_bytes().as_slice(),
                    name,
                    write.comment,
                    write.expires_at,
                    write.source_instance_id.as_bytes().as_slice(),
                    write.source_ref,
                    now,
                    now,
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        let entries = replace_entries(&mut tx, id, write.tracks, now).await?;
        for track in affected {
            refresh_playlist_pin(&mut tx, track).await?;
        }
        tx.commit().await?;
        self.signal.notify();
        Ok(Some(Playlist {
            id,
            owner_user_id: write.owner_user_id,
            name: name.to_owned(),
            comment: write.comment.map(str::to_owned),
            is_public: before.as_ref().is_some_and(|playlist| playlist.is_public),
            is_temporary: temporary,
            expires_at,
            source_instance_id: Some(write.source_instance_id),
            source_ref: Some(write.source_ref.to_owned()),
            source_instance_name: before
                .as_ref()
                .and_then(|playlist| playlist.source_instance_name.clone()),
            created_at: before.as_ref().map_or(now, |playlist| playlist.created_at),
            updated_at: now,
            entries,
        }))
    }

    /// Replaces every entry of a playlist.
    ///
    /// This is what a client that saves a queue as a playlist does: the tracks
    /// are the whole playlist afterwards.
    ///
    /// # Parameters
    ///
    /// - `id`: the playlist to change.
    /// - `editor`: the account making the change; it must own the playlist.
    /// - `tracks`: the tracks the playlist holds afterwards, in order.
    ///
    /// # Returns
    ///
    /// The playlist, with its new entries.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::UnknownPlaylist`] when no playlist has that id
    /// or the editor may not see it, [`ActivityError::NotOwner`] when it is
    /// public but belongs to another account, and the errors of
    /// [`Playlists::update`].
    pub async fn replace_tracks(
        &self,
        id: EntityId,
        editor: Uuid,
        tracks: &[EntityId],
    ) -> Result<Playlist, ActivityError> {
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        let stored = load_or_unknown(&mut tx, id).await?;
        check_owner(&stored, editor)?;
        let newly_detached = detachable(&stored);
        let mut affected = affected_tracks(&stored.entries, tracks);
        let now = now_millis();
        let entries = replace_entries(&mut tx, id, tracks, now).await?;
        detach(&mut tx, id, now, newly_detached).await?;
        sqlx::query!(
            "UPDATE playlists SET updated_at = ? WHERE id = ?",
            now,
            entity_key(id),
        )
        .execute(&mut *tx)
        .await?;
        // A playlist that just became permanent pins every track it holds,
        // including the ones it already held.
        if newly_detached {
            affected.extend(stored.track_ids());
            affected.extend(tracks.iter().copied());
            affected.sort_unstable_by_key(|track| *track.uuid().as_bytes());
            affected.dedup();
        }
        for track in affected {
            refresh_playlist_pin(&mut tx, track).await?;
        }
        tx.commit().await?;
        self.signal.notify();
        let mut playlist = stored;
        playlist.updated_at = now;
        playlist.entries = entries;
        if newly_detached {
            playlist.is_temporary = false;
            playlist.expires_at = None;
            playlist.source_instance_id = None;
            playlist.source_ref = None;
            playlist.source_instance_name = None;
        }
        Ok(playlist)
    }

    /// Reorders a playlist's entries.
    ///
    /// `order` lists the playlist's current zero-based positions in their new
    /// order: the entry at position `order[0]` becomes first, the entry at
    /// position `order[1]` second, and so on. It must be a permutation of
    /// `0..len`, because any other list would drop or duplicate an entry; the
    /// entries keep the instant they were added and every position is written
    /// in one transaction, so a reader never sees a playlist twice in a row.
    ///
    /// # Parameters
    ///
    /// - `id`: the playlist to reorder.
    /// - `editor`: the account making the change; it must own the playlist.
    /// - `order`: the current positions, in their new order.
    ///
    /// # Returns
    ///
    /// The playlist, with its entries in the new order.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::UnknownPlaylist`] when no playlist has that id
    /// or the editor may not see it, [`ActivityError::NotOwner`] when it is
    /// public but belongs to another account, [`ActivityError::InvalidOrder`]
    /// when `order` is not a permutation of `0..len`, and
    /// [`ActivityError::Database`] when a write fails.
    pub async fn reorder(
        &self,
        id: EntityId,
        editor: Uuid,
        order: &[usize],
    ) -> Result<Playlist, ActivityError> {
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        let mut playlist = load_or_unknown(&mut tx, id).await?;
        check_owner(&playlist, editor)?;
        let len = playlist.entries.len();
        if order.len() != len || !is_permutation(order, len) {
            return Err(ActivityError::InvalidOrder { len });
        }
        let newly_detached = detachable(&playlist);
        let detached_tracks = playlist.track_ids();
        let now = now_millis();
        // Every position moves out of the range the final positions occupy
        // before any of them is set, so the first primary key is never taken
        // by a row that has not moved yet.
        sqlx::query!(
            "UPDATE playlist_tracks SET position = position + ? WHERE playlist_id = ?",
            len as i64,
            entity_key(id),
        )
        .execute(&mut *tx)
        .await?;
        for (position, old) in order.iter().enumerate() {
            sqlx::query!(
                "UPDATE playlist_tracks SET position = ? WHERE playlist_id = ? AND position = ?",
                position as i64,
                entity_key(id),
                (*old as i64) + len as i64,
            )
            .execute(&mut *tx)
            .await?;
        }
        detach(&mut tx, id, now, newly_detached).await?;
        sqlx::query!(
            "UPDATE playlists SET updated_at = ? WHERE id = ?",
            now,
            entity_key(id),
        )
        .execute(&mut *tx)
        .await?;
        if newly_detached {
            for track in detached_tracks {
                refresh_playlist_pin(&mut tx, track).await?;
            }
        }
        tx.commit().await?;
        self.signal.notify();
        playlist.updated_at = now;
        if newly_detached {
            playlist.is_temporary = false;
            playlist.expires_at = None;
            playlist.source_instance_id = None;
            playlist.source_ref = None;
            playlist.source_instance_name = None;
        }
        playlist.entries = order
            .iter()
            .enumerate()
            .map(|(position, old)| {
                let mut entry = playlist.entries[*old];
                entry.position = position as i64;
                entry
            })
            .collect();
        Ok(playlist)
    }

    ///
    /// The removals are applied first, against the order the playlist had
    /// before the edit, and the additions are appended after them.
    ///
    /// # Parameters
    ///
    /// - `id`: the playlist to change.
    /// - `editor`: the account making the change; it must own the playlist.
    /// - `update`: what the edit changes.
    ///
    /// # Returns
    ///
    /// The playlist, with its new entries.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::UnknownPlaylist`] when no playlist has that id
    /// or the editor may not see it, [`ActivityError::NotOwner`] when it is
    /// public but belongs to another account, [`ActivityError::InvalidName`]
    /// when a new name is empty, and [`ActivityError::Database`] or
    /// [`ActivityError::Library`] when a write fails.
    pub async fn update(
        &self,
        id: EntityId,
        editor: Uuid,
        update: &PlaylistUpdate,
    ) -> Result<Playlist, ActivityError> {
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        let mut playlist = load_or_unknown(&mut tx, id).await?;
        check_owner(&playlist, editor)?;
        let newly_detached = detachable(&playlist);

        let mut tracks = playlist.track_ids();
        let stored_tracks = tracks.clone();
        let mut affected: Vec<EntityId> = Vec::new();
        let mut indexes = update.remove_indexes.clone();
        indexes.sort_unstable();
        indexes.dedup();
        for index in indexes.into_iter().rev() {
            if index < tracks.len() {
                affected.push(tracks.remove(index));
            }
        }
        for track in &update.add {
            tracks.push(*track);
            affected.push(*track);
        }

        let name = match &update.name {
            Some(name) => checked_name(name)?,
            None => playlist.name.as_str(),
        };
        let comment = match &update.comment {
            // An empty comment clears it; no comment leaves it alone.
            Some(comment) => Some(comment.clone()).filter(|comment| !comment.is_empty()),
            None => playlist.comment.clone(),
        };
        let public = update.public.unwrap_or(playlist.is_public);
        let now = now_millis();

        sqlx::query!(
            "UPDATE playlists SET name = ?, comment = ?, is_public = ?, updated_at = ? WHERE id = ?",
            name,
            comment,
            public,
            now,
            entity_key(id),
        )
        .execute(&mut *tx)
        .await?;
        detach(&mut tx, id, now, newly_detached).await?;
        let entries = replace_entries(&mut tx, id, &tracks, now).await?;
        affected.sort_unstable_by_key(|track| *track.uuid().as_bytes());
        affected.dedup();
        if newly_detached {
            // Every track of the playlist is pinned from now on, not only the
            // ones whose membership changed.
            affected.extend(stored_tracks);
            affected.extend(tracks.iter().copied());
            affected.sort_unstable_by_key(|track| *track.uuid().as_bytes());
            affected.dedup();
        }
        for track in affected {
            refresh_playlist_pin(&mut tx, track).await?;
        }
        tx.commit().await?;
        self.signal.notify();
        playlist.name = name.to_owned();
        playlist.comment = comment;
        playlist.is_public = public;
        playlist.updated_at = now;
        playlist.entries = entries;
        if newly_detached {
            playlist.is_temporary = false;
            playlist.expires_at = None;
            playlist.source_instance_id = None;
            playlist.source_ref = None;
            playlist.source_instance_name = None;
        }
        Ok(playlist)
    }

    /// Deletes a playlist.
    ///
    /// # Parameters
    ///
    /// - `id`: the playlist to delete.
    /// - `editor`: the account deleting it; it must own the playlist.
    ///
    /// # Returns
    ///
    /// `true` when a playlist was deleted.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::UnknownPlaylist`] when no playlist has that id
    /// or the editor may not see it, [`ActivityError::NotOwner`] when it is
    /// public but belongs to another account, and [`ActivityError::Database`]
    /// or [`ActivityError::Library`] when a write fails.
    pub async fn delete(&self, id: EntityId, editor: Uuid) -> Result<bool, ActivityError> {
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        let playlist = load_or_unknown(&mut tx, id).await?;
        check_owner(&playlist, editor)?;
        let affected = playlist.track_ids();
        let deleted = sqlx::query!("DELETE FROM playlists WHERE id = ?", entity_key(id),)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        for track in affected {
            refresh_playlist_pin(&mut tx, track).await?;
        }
        tx.commit().await?;
        self.signal.notify();
        Ok(deleted > 0)
    }

    /// Converts a playlist between temporary and permanent.
    ///
    /// A permanent playlist pins every track it holds; a temporary one pins
    /// none, so its tracks are evictable again. The new state and the playlist
    /// pin of every entry are written in one transaction, which is what keeps a
    /// conversion from leaving a track pinned by a playlist that no longer
    /// holds it.
    ///
    /// A permanent playlist has no expiry, so the expiry is cleared when the
    /// playlist is made permanent; a temporary playlist without one never
    /// expires by itself.
    ///
    /// # Parameters
    ///
    /// - `id`: the playlist to convert.
    /// - `editor`: the account making the change; it must own the playlist.
    /// - `temporary`: whether the playlist is temporary afterwards.
    /// - `expires_at`: when it expires afterwards, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// The playlist, with its new lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::UnknownPlaylist`] when no playlist has that id
    /// or the editor may not see it, [`ActivityError::NotOwner`] when it is
    /// public but belongs to another account, and [`ActivityError::Database`]
    /// or [`ActivityError::Library`] when a write fails.
    pub async fn set_lifetime(
        &self,
        id: EntityId,
        editor: Uuid,
        temporary: bool,
        expires_at: Option<i64>,
    ) -> Result<Playlist, ActivityError> {
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        let mut playlist = load_or_unknown(&mut tx, id).await?;
        check_owner(&playlist, editor)?;
        let expires_at = expires_at.filter(|_| temporary);
        let now = now_millis();
        sqlx::query!(
            "UPDATE playlists SET is_temporary = ?, expires_at = ?, updated_at = ? WHERE id = ?",
            temporary,
            expires_at,
            now,
            entity_key(id),
        )
        .execute(&mut *tx)
        .await?;
        for track in playlist.track_ids() {
            refresh_playlist_pin(&mut tx, track).await?;
        }
        tx.commit().await?;
        self.signal.notify();
        playlist.is_temporary = temporary;
        playlist.expires_at = expires_at;
        playlist.updated_at = now;
        Ok(playlist)
    }

    /// Deletes the generated playlists of one instance that are still
    /// temporary.
    ///
    /// Removing a discovery instance must not leave the playlists it produced
    /// behind with no source: they would linger until their expiry, and a
    /// re-added instance would create a second copy next to them. So the
    /// instance's still-temporary playlists go with it, in one transaction that
    /// also refreshes the playlist pin of every track they held.
    ///
    /// A playlist the listener made permanent, or one their first edit detached,
    /// is kept: it is their playlist even though the slot's source is gone.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the provider instance being removed.
    ///
    /// # Returns
    ///
    /// The deleted playlists as (owner, playlist id), so the caller can tell
    /// each owner that their playlist is gone.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read or
    /// deleted and [`ActivityError::Library`] when a pin cannot be updated.
    pub async fn delete_generated_for_instance(
        &self,
        instance_id: Uuid,
    ) -> Result<Vec<(Uuid, EntityId)>, ActivityError> {
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        let rows = sqlx::query!(
            "SELECT id, owner_user_id FROM playlists \
             WHERE source_instance_id = ? AND is_temporary = 1",
            instance_id.as_bytes().as_slice(),
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut deleted = Vec::with_capacity(rows.len());
        let mut tracks = Vec::new();
        for row in rows {
            let id = EntityId::from_uuid(IdPrefix::Playlist, Uuid::from_slice(&row.id)?);
            let owner = Uuid::from_slice(&row.owner_user_id)?;
            let entries = sqlx::query_scalar!(
                "SELECT track_id FROM playlist_tracks WHERE playlist_id = ?",
                row.id.as_slice(),
            )
            .fetch_all(&mut *tx)
            .await?;
            for track in entries {
                tracks.push(EntityId::from_uuid(
                    IdPrefix::Track,
                    Uuid::from_slice(&track)?,
                ));
            }
            sqlx::query!("DELETE FROM playlists WHERE id = ?", row.id.as_slice())
                .execute(&mut *tx)
                .await?;
            deleted.push((owner, id));
        }
        tracks.sort_unstable_by_key(|track| *track.uuid().as_bytes());
        tracks.dedup();
        for track in tracks {
            refresh_playlist_pin(&mut tx, track).await?;
        }
        tx.commit().await?;
        if !deleted.is_empty() {
            self.signal.notify();
        }
        Ok(deleted)
    }

    /// Deletes the temporary playlists whose expiry has passed.
    ///
    /// The entries go with the playlist, and the playlist pin of every track
    /// they held is refreshed in the same transaction. A temporary playlist
    /// normally pins nothing, but a row an earlier bug left pinned by one would
    /// otherwise stay pinned by a playlist that no longer exists; refreshing
    /// re-derives each track from the playlists that remain.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant to compare the expiries against, in unix
    ///   milliseconds.
    ///
    /// # Returns
    ///
    /// How many playlists were deleted.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read or
    /// deleted and [`ActivityError::Library`] when a pin cannot be updated.
    pub async fn delete_expired(&self, now: i64) -> Result<u64, ActivityError> {
        let mut tx = self.writer.begin_with(crate::READ_THEN_WRITE).await?;
        let tracks = sqlx::query_scalar!(
            "SELECT DISTINCT pt.track_id FROM playlist_tracks pt \
             JOIN playlists p ON p.id = pt.playlist_id \
             WHERE p.is_temporary = 1 AND p.expires_at IS NOT NULL AND p.expires_at <= ?",
            now,
        )
        .fetch_all(&mut *tx)
        .await?;
        let deleted = sqlx::query!(
            "DELETE FROM playlists \
             WHERE is_temporary = 1 AND expires_at IS NOT NULL AND expires_at <= ?",
            now,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        for track in tracks {
            let track = EntityId::from_uuid(IdPrefix::Track, Uuid::from_slice(&track)?);
            refresh_playlist_pin(&mut tx, track).await?;
        }
        tx.commit().await?;
        if deleted > 0 {
            self.signal.notify();
        }
        Ok(deleted)
    }

    /// Reads one playlist.
    ///
    /// # Parameters
    ///
    /// - `id`: the playlist to read.
    /// - `viewer`: the account reading it. A playlist that is neither the
    ///   viewer's nor public is reported as missing, so one client cannot learn
    ///   that another user's private playlist exists.
    ///
    /// # Returns
    ///
    /// The playlist, or `None` when it does not exist or the viewer may not see
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read.
    pub async fn get(&self, id: EntityId, viewer: Uuid) -> Result<Option<Playlist>, ActivityError> {
        let mut conn = self.readers.acquire().await?;
        let Some(playlist) = load(&mut conn, id).await? else {
            return Ok(None);
        };
        if playlist.owner_user_id != viewer && !playlist.is_public {
            return Ok(None);
        }
        Ok(Some(playlist))
    }

    /// Lists the playlists a viewer may see.
    ///
    /// # Parameters
    ///
    /// - `viewer`: the account reading. A playlist is listed when the viewer
    ///   owns it or it is public.
    /// - `owner_filter`: when set, only that owner's playlists are considered.
    ///   The filter narrows the result; it never widens it, so a caller that
    ///   offers another user's playlists checks the right to ask first.
    ///
    /// # Returns
    ///
    /// The playlists, in name order and then by id, each with its entries.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read.
    pub async fn list(
        &self,
        viewer: Uuid,
        owner_filter: Option<Uuid>,
    ) -> Result<Vec<Playlist>, ActivityError> {
        let mut conn = self.readers.acquire().await?;
        let rows = sqlx::query_as!(
            PlaylistRow,
            "SELECT p.id, p.owner_user_id, p.name, p.comment, p.is_public, p.is_temporary, \
                    p.expires_at, p.source_instance_id, p.source_ref, p.detached_at, \
                    p.created_at, p.updated_at, pi.display_name AS source_instance_name \
             FROM playlists p \
             LEFT JOIN provider_instances pi ON pi.id = p.source_instance_id \
             WHERE (p.owner_user_id = ?1 OR p.is_public = 1) \
               AND (?2 IS NULL OR p.owner_user_id = ?2) \
             ORDER BY p.name, p.id",
            viewer.as_bytes().as_slice(),
            owner_filter.map(|owner| owner.as_bytes().to_vec()),
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut playlists = Vec::with_capacity(rows.len());
        for row in rows {
            playlists.push(load_entries(&mut conn, row).await?);
        }
        Ok(playlists)
    }
}

/// Returns a track's primary key bytes as an owned vector for a query bind.
///
/// Binding an owned vector rather than a slice of a temporary keeps the
/// compile-checked query macros free of borrow-lifetime noise.
///
/// # Parameters
///
/// - `id`: the entity's client id.
///
/// # Returns
///
/// The 16 bytes the `BLOB` column holds.
fn entity_key(id: EntityId) -> Vec<u8> {
    id.uuid().as_bytes().to_vec()
}

/// Converts a stored playlist row into a playlist without its entries.
///
/// A generated playlist the listener has edited keeps the columns that identify
/// its service slot, so the next sync recognises it, but it is the listener's
/// playlist: it reads back with no source at all, which is what makes
/// [`Playlist::source_instance_name`] mean "generated and not yet detached".
///
/// # Parameters
///
/// - `row`: the stored row.
///
/// # Errors
///
/// Returns [`ActivityError::CorruptId`] when a stored id is not 16 bytes.
async fn decode_row(row: PlaylistRow) -> Result<Playlist, ActivityError> {
    let detached = row.detached_at.is_some();
    Ok(Playlist {
        id: EntityId::from_uuid(IdPrefix::Playlist, Uuid::from_slice(&row.id)?),
        owner_user_id: Uuid::from_slice(&row.owner_user_id)?,
        name: row.name,
        comment: row.comment,
        is_public: row.is_public != 0,
        is_temporary: row.is_temporary != 0,
        expires_at: row.expires_at,
        source_instance_id: row
            .source_instance_id
            .filter(|_| !detached)
            .map(|id| Uuid::from_slice(&id))
            .transpose()?,
        source_ref: row.source_ref.filter(|_| !detached),
        source_instance_name: row.source_instance_name.filter(|_| !detached),
        created_at: row.created_at,
        updated_at: row.updated_at,
        entries: Vec::new(),
    })
}

/// Reads a playlist with its entries, or reports the id as unknown.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `id`: the playlist to read.
///
/// # Errors
///
/// Returns [`ActivityError::UnknownPlaylist`] when no playlist has that id.
async fn load_or_unknown(
    conn: &mut SqliteConnection,
    id: EntityId,
) -> Result<Playlist, ActivityError> {
    load(conn, id)
        .await?
        .ok_or_else(|| ActivityError::UnknownPlaylist { id: id.to_string() })
}

/// Reads a playlist with its entries.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `id`: the playlist to read.
///
/// # Returns
///
/// The playlist, or `None` when no playlist has that id.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the rows cannot be read.
async fn load(
    conn: &mut SqliteConnection,
    id: EntityId,
) -> Result<Option<Playlist>, ActivityError> {
    let row = sqlx::query_as!(
        PlaylistRow,
        "SELECT p.id, p.owner_user_id, p.name, p.comment, p.is_public, p.is_temporary, \
                p.expires_at, p.source_instance_id, p.source_ref, p.detached_at, \
                p.created_at, p.updated_at, pi.display_name AS source_instance_name \
         FROM playlists p \
         LEFT JOIN provider_instances pi ON pi.id = p.source_instance_id \
         WHERE p.id = ?",
        entity_key(id),
    )
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some(row) => load_entries(conn, row).await.map(Some),
        None => Ok(None),
    }
}

/// Reads the entries of one playlist.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `row`: the playlist's stored row.
///
/// # Errors
///
/// Returns [`ActivityError`] when the rows cannot be read or an id does not
/// decode.
async fn load_entries(
    conn: &mut SqliteConnection,
    row: PlaylistRow,
) -> Result<Playlist, ActivityError> {
    let mut playlist = decode_row(row).await?;
    let rows = sqlx::query_as!(
        EntryRow,
        "SELECT position, track_id, added_at FROM playlist_tracks \
         WHERE playlist_id = ? ORDER BY position",
        entity_key(playlist.id),
    )
    .fetch_all(&mut *conn)
    .await?;
    playlist.entries = rows
        .into_iter()
        .map(|entry| {
            Ok(PlaylistEntry {
                track_id: EntityId::from_uuid(IdPrefix::Track, Uuid::from_slice(&entry.track_id)?),
                position: entry.position,
                added_at: entry.added_at,
            })
        })
        .collect::<Result<Vec<_>, uuid::Error>>()?;
    Ok(playlist)
}

/// Reports an empty or over-long name.
///
/// # Parameters
///
/// - `name`: the name a caller supplied.
///
/// # Returns
///
/// The name to store.
///
/// # Errors
///
/// Returns [`ActivityError::InvalidName`] when the name is empty or longer than
/// [`MAX_PLAYLIST_NAME`].
fn checked_name(name: &str) -> Result<&str, ActivityError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_PLAYLIST_NAME {
        return Err(ActivityError::InvalidName {
            name: name.to_owned(),
            max: MAX_PLAYLIST_NAME,
        });
    }
    Ok(trimmed)
}

/// Reports an edit by a caller that does not own the playlist.
///
/// A playlist the caller may not see is reported as missing, so no id can be
/// probed: `deletePlaylist` answers what `getPlaylist` answers. A public
/// playlist the caller can see but does not own is reported as another
/// account's, which is a fact the listing already told them.
///
/// # Parameters
///
/// - `playlist`: the playlist being changed.
/// - `editor`: the account making the change.
///
/// # Errors
///
/// Returns [`ActivityError::NotOwner`] when the playlist is public and
/// [`ActivityError::UnknownPlaylist`] when the caller may not see it.
fn check_owner(playlist: &Playlist, editor: Uuid) -> Result<(), ActivityError> {
    if playlist.owner_user_id == editor {
        return Ok(());
    }
    if playlist.is_public {
        return Err(ActivityError::NotOwner {
            id: playlist.id.to_string(),
        });
    }
    Err(ActivityError::UnknownPlaylist {
        id: playlist.id.to_string(),
    })
}

/// Reports whether an edit takes a generated playlist away from its service.
///
/// A playlist is still generated while it names the instance that produced it:
/// [`decode_row`] clears that for a playlist the listener already edited, so the
/// first edit is the one that sees a source.
///
/// # Parameters
///
/// - `playlist`: the playlist as it was loaded.
///
/// # Returns
///
/// `true` when this edit is the one that makes the playlist permanent and the
/// listener's own.
fn detachable(playlist: &Playlist) -> bool {
    playlist.source_instance_name.is_some()
}

/// Makes a generated playlist the listener's own.
///
/// The slot columns stay, so the next sync recognises the playlist and leaves it
/// alone instead of creating a copy, but it stops being temporary, loses its
/// expiry, and records when the edit happened.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the playlist being edited.
/// - `now`: when the edit happened, in unix milliseconds.
/// - `detach`: whether the playlist has to be detached at all.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the row cannot be written.
async fn detach(
    conn: &mut SqliteConnection,
    id: EntityId,
    now: i64,
    detach: bool,
) -> Result<(), ActivityError> {
    if !detach {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE playlists \
         SET is_temporary = 0, expires_at = NULL, detached_at = COALESCE(detached_at, ?) \
         WHERE id = ?",
        now,
        entity_key(id),
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Reports whether a reorder lists every position of a playlist exactly once.
///
/// # Parameters
///
/// - `order`: the positions the caller listed, in their new order.
/// - `len`: how many entries the playlist holds.
///
/// # Returns
///
/// `true` when `order` holds every value from `0` to `len` exactly once.
fn is_permutation(order: &[usize], len: usize) -> bool {
    let mut seen = vec![false; len];
    for position in order {
        if *position >= len || seen[*position] {
            return false;
        }
        seen[*position] = true;
    }
    true
}

/// Returns the tracks whose membership changes between two entry lists.
///
/// # Parameters
///
/// - `before`: the entries as they were.
/// - `after`: the tracks the playlist holds afterwards.
///
/// # Returns
///
/// Every track in either list that is not in both, once each.
fn affected_tracks(before: &[PlaylistEntry], after: &[EntityId]) -> Vec<EntityId> {
    let mut affected: Vec<EntityId> = Vec::new();
    for entry in before {
        if !after.contains(&entry.track_id) {
            affected.push(entry.track_id);
        }
    }
    for track in after {
        if !before.iter().any(|entry| entry.track_id == *track) {
            affected.push(*track);
        }
    }
    affected.sort_unstable_by_key(|track| *track.uuid().as_bytes());
    affected.dedup();
    affected
}

/// Writes a playlist's entries from scratch, renumbering them from zero.
///
/// The entries that survive keep the instant they were added; the new ones get
/// `now`. Writing them all keeps the positions dense, which is what a position
/// means.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `playlist_id`: the playlist to write.
/// - `tracks`: the tracks the playlist holds afterwards, in order.
/// - `now`: the instant the new entries were added, in unix milliseconds.
///
/// # Returns
///
/// The entries as they were written.
///
/// # Errors
///
/// Returns [`ActivityError`] when the rows cannot be read or written.
async fn replace_entries(
    conn: &mut SqliteConnection,
    playlist_id: EntityId,
    tracks: &[EntityId],
    now: i64,
) -> Result<Vec<PlaylistEntry>, ActivityError> {
    let existing = sqlx::query_as!(
        EntryRow,
        "SELECT position, track_id, added_at FROM playlist_tracks \
         WHERE playlist_id = ? ORDER BY position",
        entity_key(playlist_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    let added_at = |track: EntityId| {
        existing
            .iter()
            .find(|row| row.track_id == track.uuid().as_bytes())
            .map_or(now, |row| row.added_at)
    };
    sqlx::query!(
        "DELETE FROM playlist_tracks WHERE playlist_id = ?",
        entity_key(playlist_id),
    )
    .execute(&mut *conn)
    .await?;
    let mut entries = Vec::with_capacity(tracks.len());
    for (position, track) in tracks.iter().enumerate() {
        let position = position as i64;
        sqlx::query!(
            "INSERT INTO playlist_tracks (playlist_id, position, track_id, added_at) \
             VALUES (?, ?, ?, ?)",
            entity_key(playlist_id),
            position,
            entity_key(*track),
            added_at(*track),
        )
        .execute(&mut *conn)
        .await?;
        entries.push(PlaylistEntry {
            track_id: *track,
            position,
            added_at: added_at(*track),
        });
    }
    Ok(entries)
}

/// Re-derives the playlist pin of one track.
///
/// The pin is set while the track is in any non-temporary playlist, and removed
/// otherwise. The availability the pin implies is recomputed by the retention
/// layer.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to re-derive.
///
/// # Returns
///
/// `true` when the track is in a playlist.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the playlists cannot be read and
/// [`ActivityError::Library`] when the pin or the availability cannot be
/// written.
pub async fn refresh_playlist_pin(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<bool, ActivityError> {
    let memberships: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
         WHERE pt.track_id = ? AND p.is_temporary = 0",
    )
    .bind(entity_key(track_id))
    .fetch_one(&mut *conn)
    .await?;
    let wanted = memberships > 0;

    let pinned = retention::pin_reasons(conn, track_id)
        .await?
        .contains(&PinReason::Playlist);
    if wanted && !pinned {
        retention::add_pin_reason(conn, track_id, PinReason::Playlist).await?;
    } else if !wanted && pinned {
        retention::remove_pin_reason(conn, track_id, PinReason::Playlist).await?;
    }
    Ok(wanted)
}

/// The job kind of the playlist expiry sweep.
pub const EXPIRE_JOB_KIND: &str = "playlists.expire";

/// The cron expression of the playlist expiry sweep: every 15 minutes.
///
/// A temporary playlist is one the server produced for a bounded purpose, so an
/// expiry that has passed is a small cleanup rather than an urgent one; a
/// quarter-hour cadence keeps the table from holding playlists nobody wants
/// without waking the process often.
pub const EXPIRE_SCHEDULE: &str = "*/15 * * * *";

/// Deletes the temporary playlists whose expiry has passed for the
/// `playlists.expire` job.
///
/// The server registers it under [`EXPIRE_JOB_KIND`] and schedules it at
/// [`EXPIRE_SCHEDULE`], so a temporary playlist that nobody converts or deletes
/// goes away on its own.
#[derive(Debug, Clone)]
pub struct ExpireJobHandler {
    /// The playlist repository the sweep runs through.
    playlists: Playlists,
}

impl ExpireJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `playlists`: the playlist repository the sweep runs through.
    ///
    /// # Returns
    ///
    /// The handler.
    pub fn new(playlists: Playlists) -> ExpireJobHandler {
        ExpireJobHandler { playlists }
    }
}

#[async_trait]
impl JobHandler for ExpireJobHandler {
    /// Deletes every temporary playlist whose expiry has passed.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job, used for its kind in the log line.
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the delete fails, so the queue retries it on
    /// its retry schedule.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        match self.playlists.delete_expired(now_millis()).await {
            Ok(deleted) => {
                tracing::debug!(kind = %job.kind, deleted, "swept expired temporary playlists");
                Ok(())
            }
            Err(error) => Err(JobError::new(error.to_string())),
        }
    }
}
