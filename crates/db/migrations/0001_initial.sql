-- Initial schema: every table of the on-demand vertical slice.
--
-- Tables follow the storage conventions of the data model: STRICT tables, UUIDv7 BLOB keys,
-- unix-millisecond INTEGER timestamps, 0/1 INTEGER booleans with CHECK constraints, and JSON
-- held in TEXT columns. Foreign keys are enforced per connection by the database layer.

-- users
CREATE TABLE users (
  id            BLOB    PRIMARY KEY,
  username      TEXT    NOT NULL,
  display_name  TEXT    NOT NULL,
  password_hash TEXT    NOT NULL,
  is_admin      INTEGER NOT NULL DEFAULT 0 CHECK (is_admin IN (0, 1)),
  can_acquire   INTEGER NOT NULL DEFAULT 1 CHECK (can_acquire IN (0, 1)),
  can_upload    INTEGER NOT NULL DEFAULT 1 CHECK (can_upload IN (0, 1)),
  can_download  INTEGER NOT NULL DEFAULT 1 CHECK (can_download IN (0, 1)),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  disabled_at   INTEGER
) STRICT;

CREATE UNIQUE INDEX users_username_uq ON users (username COLLATE NOCASE);

-- api_keys
CREATE TABLE api_keys (
  id           BLOB    PRIMARY KEY,
  user_id      BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  name         TEXT    NOT NULL,
  key_prefix   TEXT    NOT NULL,
  key_hash     BLOB    NOT NULL,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER
) STRICT;

CREATE UNIQUE INDEX api_keys_key_hash_uq ON api_keys (key_hash);
CREATE INDEX api_keys_user_idx ON api_keys (user_id);

-- app_passwords
CREATE TABLE app_passwords (
  id           BLOB    PRIMARY KEY,
  user_id      BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  name         TEXT    NOT NULL,
  ciphertext   BLOB    NOT NULL,
  nonce        BLOB    NOT NULL,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER
) STRICT;

CREATE INDEX app_passwords_user_idx ON app_passwords (user_id);

-- settings
CREATE TABLE settings (
  key        TEXT    PRIMARY KEY,
  value      TEXT    NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by BLOB    REFERENCES users (id) ON DELETE SET NULL
) STRICT;

-- user_settings
CREATE TABLE user_settings (
  user_id BLOB NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  key     TEXT NOT NULL,
  value   TEXT NOT NULL,
  PRIMARY KEY (user_id, key)
) STRICT;

-- library_folders
CREATE TABLE library_folders (
  id            BLOB    PRIMARY KEY,
  path          TEXT    NOT NULL,
  name          TEXT    NOT NULL,
  owner_user_id BLOB    REFERENCES users (id) ON DELETE SET NULL,
  watch         INTEGER NOT NULL DEFAULT 1 CHECK (watch IN (0, 1)),
  created_at    INTEGER NOT NULL,
  last_scan_at  INTEGER
) STRICT;

CREATE UNIQUE INDEX library_folders_path_uq ON library_folders (path);
CREATE INDEX library_folders_owner_idx ON library_folders (owner_user_id);

-- artists
CREATE TABLE artists (
  id           BLOB    PRIMARY KEY,
  name         TEXT    NOT NULL,
  sort_name    TEXT    NOT NULL,
  mb_artist_id TEXT,
  origin       TEXT    NOT NULL CHECK (origin IN ('local', 'catalog')),
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
) STRICT;

CREATE INDEX artists_mb_artist_id_idx ON artists (mb_artist_id);
CREATE INDEX artists_sort_name_idx ON artists (sort_name);

-- albums
CREATE TABLE albums (
  id                  BLOB    PRIMARY KEY,
  title               TEXT    NOT NULL,
  sort_title          TEXT    NOT NULL,
  display_artist      TEXT,
  year                INTEGER,
  release_date        TEXT,
  album_type          TEXT,
  mb_release_id       TEXT,
  mb_release_group_id TEXT,
  upc                 TEXT,
  origin              TEXT    NOT NULL CHECK (origin IN ('local', 'catalog')),
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL
) STRICT;

CREATE INDEX albums_mb_release_id_idx ON albums (mb_release_id);
CREATE INDEX albums_mb_release_group_idx ON albums (mb_release_group_id);
CREATE INDEX albums_sort_title_idx ON albums (sort_title);

-- album_artists
CREATE TABLE album_artists (
  album_id  BLOB    NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
  artist_id BLOB    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
  position  INTEGER NOT NULL,
  role      TEXT    NOT NULL,
  PRIMARY KEY (album_id, position)
) STRICT;

CREATE INDEX album_artists_artist_idx ON album_artists (artist_id);

-- tracks
CREATE TABLE tracks (
  id                BLOB    PRIMARY KEY,
  album_id          BLOB    REFERENCES albums (id) ON DELETE SET NULL,
  title             TEXT    NOT NULL,
  sort_title        TEXT    NOT NULL,
  display_artist    TEXT,
  disc_number       INTEGER,
  track_number      INTEGER,
  duration_ms       INTEGER,
  isrc              TEXT,
  mb_recording_id   TEXT,
  explicit          INTEGER CHECK (explicit IN (0, 1)),
  origin            TEXT    NOT NULL CHECK (origin IN ('local', 'catalog')),
  availability      TEXT    NOT NULL CHECK (availability IN ('local', 'cached', 'acquiring', 'remote', 'unavailable')),
  unavailable_until INTEGER,
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
) STRICT;

CREATE INDEX tracks_album_idx ON tracks (album_id, disc_number, track_number);
CREATE INDEX tracks_isrc_idx ON tracks (isrc);
CREATE INDEX tracks_mb_recording_idx ON tracks (mb_recording_id);
CREATE INDEX tracks_availability_idx ON tracks (availability, unavailable_until);

-- track_artists
CREATE TABLE track_artists (
  track_id  BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  artist_id BLOB    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
  position  INTEGER NOT NULL,
  role      TEXT    NOT NULL,
  PRIMARY KEY (track_id, position)
) STRICT;

CREATE INDEX track_artists_artist_idx ON track_artists (artist_id);

-- genres
CREATE TABLE genres (
  id   BLOB PRIMARY KEY,
  name TEXT NOT NULL
) STRICT;

CREATE UNIQUE INDEX genres_name_uq ON genres (name);

-- track_genres
CREATE TABLE track_genres (
  track_id BLOB NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  genre_id BLOB NOT NULL REFERENCES genres (id) ON DELETE CASCADE,
  PRIMARY KEY (track_id, genre_id)
) STRICT;

CREATE INDEX track_genres_genre_idx ON track_genres (genre_id);

-- external_ids
CREATE TABLE external_ids (
  namespace   TEXT NOT NULL,
  value       TEXT NOT NULL,
  entity_kind TEXT NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist', 'playlist')),
  entity_id   BLOB NOT NULL,
  PRIMARY KEY (namespace, entity_kind, value)
) STRICT;

CREATE INDEX external_ids_entity_idx ON external_ids (entity_kind, entity_id);

-- media_files
CREATE TABLE media_files (
  id               BLOB    PRIMARY KEY,
  track_id         BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  storage          TEXT    NOT NULL CHECK (storage IN ('folder', 'library', 'cache')),
  folder_id        BLOB    REFERENCES library_folders (id) ON DELETE CASCADE,
  rel_path         TEXT    NOT NULL,
  size             INTEGER NOT NULL,
  mtime_ms         INTEGER NOT NULL,
  format           TEXT,
  codec            TEXT,
  bitrate_kbps     INTEGER,
  sample_rate      INTEGER,
  bit_depth        INTEGER,
  channels         INTEGER,
  duration_ms      INTEGER,
  lossless         INTEGER CHECK (lossless IN (0, 1)),
  quality_tier     TEXT,
  replaygain       TEXT,
  has_embedded_art INTEGER CHECK (has_embedded_art IN (0, 1)),
  added_at         INTEGER NOT NULL,
  last_accessed_at INTEGER,
  uploaded_by      BLOB    REFERENCES users (id) ON DELETE SET NULL,
  acquisition_id   BLOB    REFERENCES acquisitions (id) ON DELETE SET NULL,
  is_preferred     INTEGER NOT NULL DEFAULT 0 CHECK (is_preferred IN (0, 1))
) STRICT;

CREATE UNIQUE INDEX media_files_storage_path_uq ON media_files (storage, folder_id, rel_path);
CREATE UNIQUE INDEX media_files_managed_path_uq ON media_files (storage, rel_path)
  WHERE storage IN ('library', 'cache');
CREATE INDEX media_files_track_idx ON media_files (track_id, is_preferred);
CREATE INDEX media_files_cache_idx ON media_files (storage, last_accessed_at);

-- track_retention
CREATE TABLE track_retention (
  track_id    BLOB    PRIMARY KEY REFERENCES tracks (id) ON DELETE CASCADE,
  pinned      INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
  pin_reasons TEXT    NOT NULL DEFAULT '[]',
  updated_at  INTEGER NOT NULL
) STRICT;

CREATE INDEX track_retention_pinned_idx ON track_retention (pinned);

-- user_library
CREATE TABLE user_library (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist', 'playlist')),
  entity_id   BLOB    NOT NULL,
  origin      TEXT    NOT NULL CHECK (origin IN ('saved', 'upload')),
  added_at    INTEGER NOT NULL,
  PRIMARY KEY (user_id, entity_kind, entity_id, origin)
) STRICT;

CREATE INDEX user_library_entity_idx ON user_library (entity_kind, entity_id);

-- user_feedback
CREATE TABLE user_feedback (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist')),
  entity_id   BLOB    NOT NULL,
  value       TEXT    NOT NULL CHECK (value IN ('like', 'dislike')),
  at          INTEGER NOT NULL,
  PRIMARY KEY (user_id, entity_kind, entity_id)
) STRICT;

CREATE INDEX user_feedback_entity_idx ON user_feedback (entity_kind, entity_id, value);

-- plays
CREATE TABLE plays (
  id              BLOB    PRIMARY KEY,
  user_id         BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  track_id        BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  client          TEXT,
  started_at      INTEGER NOT NULL,
  listened_ms     INTEGER NOT NULL,
  audio_delivered INTEGER NOT NULL CHECK (audio_delivered IN (0, 1)),
  scrobbled       INTEGER NOT NULL DEFAULT 0 CHECK (scrobbled IN (0, 1))
) STRICT;

CREATE INDEX plays_user_track_idx ON plays (user_id, track_id, started_at);
CREATE INDEX plays_started_idx ON plays (started_at);

-- playlists
CREATE TABLE playlists (
  id                 BLOB    PRIMARY KEY,
  owner_user_id      BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  name               TEXT    NOT NULL,
  comment            TEXT,
  is_public          INTEGER NOT NULL DEFAULT 0 CHECK (is_public IN (0, 1)),
  is_temporary       INTEGER NOT NULL DEFAULT 0 CHECK (is_temporary IN (0, 1)),
  expires_at         INTEGER,
  source_instance_id BLOB    REFERENCES provider_instances (id) ON DELETE SET NULL,
  source_ref         TEXT,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
) STRICT;

CREATE INDEX playlists_owner_idx ON playlists (owner_user_id);
CREATE INDEX playlists_temporary_idx ON playlists (is_temporary, expires_at);

-- playlist_tracks
CREATE TABLE playlist_tracks (
  playlist_id BLOB    NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
  position    INTEGER NOT NULL,
  track_id    BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  added_at    INTEGER NOT NULL,
  PRIMARY KEY (playlist_id, position)
) STRICT;

CREATE INDEX playlist_tracks_track_idx ON playlist_tracks (track_id);

-- acquisitions
CREATE TABLE acquisitions (
  id             BLOB    PRIMARY KEY,
  track_id       BLOB    REFERENCES tracks (id) ON DELETE SET NULL,
  album_id       BLOB    REFERENCES albums (id) ON DELETE SET NULL,
  granularity    TEXT    NOT NULL CHECK (granularity IN ('track', 'album')),
  priority       TEXT    NOT NULL CHECK (priority IN ('play', 'next', 'queue', 'background', 'upgrade')),
  reason         TEXT    NOT NULL,
  state          TEXT    NOT NULL CHECK (state IN ('queued', 'resolving', 'searching', 'downloading', 'verifying', 'importing', 'completed', 'failed', 'cancelled')),
  requested_by   BLOB    REFERENCES users (id) ON DELETE SET NULL,
  instance_id    BLOB    REFERENCES provider_instances (id) ON DELETE SET NULL,
  candidate      TEXT,
  plugin_token   TEXT,
  progress       TEXT,
  attempts       INTEGER NOT NULL DEFAULT 0,
  failure_reason TEXT,
  error_detail   TEXT,
  media_file_id  BLOB    REFERENCES media_files (id) ON DELETE SET NULL,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
) STRICT;

CREATE INDEX acquisitions_state_idx ON acquisitions (state, priority);
CREATE INDEX acquisitions_track_idx ON acquisitions (track_id);
CREATE INDEX acquisitions_album_idx ON acquisitions (album_id);

-- acquisition_log
CREATE TABLE acquisition_log (
  acquisition_id BLOB    NOT NULL REFERENCES acquisitions (id) ON DELETE CASCADE,
  at             INTEGER NOT NULL,
  state          TEXT    NOT NULL,
  message        TEXT
) STRICT;

CREATE INDEX acquisition_log_acq_idx ON acquisition_log (acquisition_id, at);

-- plugin_repositories
--
-- Created here rather than with the repository installer because `plugins.repository_id`
-- references it: SQLite rejects every write to a child table whose parent table is missing,
-- even when the foreign key value is NULL.
CREATE TABLE plugin_repositories (
  id              BLOB    PRIMARY KEY,
  url             TEXT    NOT NULL,
  name            TEXT,
  public_key      TEXT,
  enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
  last_fetched_at INTEGER,
  last_error      TEXT
) STRICT;

CREATE UNIQUE INDEX plugin_repositories_url_uq ON plugin_repositories (url);

-- plugins
CREATE TABLE plugins (
  id                   TEXT    PRIMARY KEY,
  version              TEXT    NOT NULL,
  api                  TEXT    NOT NULL,
  manifest             TEXT    NOT NULL,
  sha256               BLOB    NOT NULL,
  source               TEXT    NOT NULL CHECK (source IN ('official', 'repository', 'sideload', 'bundled')),
  repository_id        BLOB    REFERENCES plugin_repositories (id) ON DELETE SET NULL,
  approved_permissions TEXT,
  enabled              INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
  installed_at         INTEGER NOT NULL
) STRICT;

CREATE INDEX plugins_enabled_idx ON plugins (enabled);

-- provider_instances
CREATE TABLE provider_instances (
  id            BLOB    PRIMARY KEY,
  plugin_id     TEXT    NOT NULL REFERENCES plugins (id) ON DELETE CASCADE,
  slug          TEXT    NOT NULL,
  display_name  TEXT    NOT NULL,
  server_config TEXT,
  host_config   TEXT,
  enabled       INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
  health        TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
) STRICT;

CREATE UNIQUE INDEX provider_instances_slug_uq ON provider_instances (slug);
CREATE INDEX provider_instances_plugin_idx ON provider_instances (plugin_id);

-- layer_assignments
CREATE TABLE layer_assignments (
  layer       TEXT    NOT NULL CHECK (layer IN ('catalog', 'metadata', 'artwork', 'acquisition', 'verification', 'scrobble', 'discovery', 'lyrics', 'playlist-import')),
  position    INTEGER NOT NULL,
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  PRIMARY KEY (layer, position)
) STRICT;

CREATE INDEX layer_assignments_instance_idx ON layer_assignments (instance_id);

-- plugin_kv
CREATE TABLE plugin_kv (
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  key         TEXT    NOT NULL,
  value       BLOB    NOT NULL,
  expires_at  INTEGER,
  PRIMARY KEY (instance_id, key)
) STRICT;

CREATE INDEX plugin_kv_expiry_idx ON plugin_kv (expires_at);

-- jobs
CREATE TABLE jobs (
  id           BLOB    PRIMARY KEY,
  kind         TEXT    NOT NULL,
  payload      TEXT,
  state        TEXT    NOT NULL,
  priority     INTEGER NOT NULL,
  attempts     INTEGER NOT NULL DEFAULT 0,
  run_at       INTEGER NOT NULL,
  locked_until INTEGER,
  last_error   TEXT,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
) STRICT;

CREATE INDEX jobs_claim_idx ON jobs (state, run_at, priority);

-- schedules
CREATE TABLE schedules (
  id            BLOB    PRIMARY KEY,
  owner_user_id BLOB    REFERENCES users (id) ON DELETE CASCADE,
  kind          TEXT    NOT NULL,
  cron          TEXT    NOT NULL,
  payload       TEXT,
  enabled       INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
  next_run_at   INTEGER,
  last_run_at   INTEGER
) STRICT;

CREATE INDEX schedules_due_idx ON schedules (enabled, next_run_at);

-- catalog_cache
CREATE TABLE catalog_cache (
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  request_key TEXT    NOT NULL,
  response    TEXT    NOT NULL,
  fetched_at  INTEGER NOT NULL,
  expires_at  INTEGER,
  PRIMARY KEY (instance_id, request_key)
) STRICT;

CREATE INDEX catalog_cache_expiry_idx ON catalog_cache (expires_at);

-- artwork_cache
CREATE TABLE artwork_cache (
  key          TEXT    PRIMARY KEY,
  source       TEXT    NOT NULL,
  path         TEXT    NOT NULL,
  content_type TEXT    NOT NULL,
  width        INTEGER,
  height       INTEGER,
  fetched_at   INTEGER NOT NULL
) STRICT;

-- search_fts
--
-- One contentless FTS5 index serves tracks, albums and artists. Each FTS row id is derived from
-- the source row id as `rowid * 4 + kind` (track 1, album 2, artist 3), so the three tables
-- never collide and the triggers can delete the exact row they inserted. `contentless_delete`
-- allows plain DELETE by rowid, and `contentless_unindexed` keeps `entity_kind` and `entity_id`
-- readable so search results resolve to entities without decoding the rowid.
--
-- The derived rowids depend on the implicit rowids of the source tables, which an in-place
-- VACUUM may renumber. The server never runs VACUUM on the live database (backups use
-- VACUUM INTO, which leaves the source untouched).
CREATE VIRTUAL TABLE search_fts USING fts5(
  entity_kind UNINDEXED,
  entity_id   UNINDEXED,
  title,
  subtitle,
  content = '',
  contentless_delete = 1,
  contentless_unindexed = 1,
  tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER tracks_fts_ai AFTER INSERT ON tracks BEGIN
  INSERT INTO search_fts (rowid, entity_kind, entity_id, title, subtitle)
  VALUES (new.rowid * 4 + 1, 'track', new.id, new.title, new.display_artist);
END;

CREATE TRIGGER tracks_fts_ad AFTER DELETE ON tracks BEGIN
  DELETE FROM search_fts WHERE rowid = old.rowid * 4 + 1;
END;

CREATE TRIGGER tracks_fts_au AFTER UPDATE OF title, display_artist ON tracks BEGIN
  DELETE FROM search_fts WHERE rowid = old.rowid * 4 + 1;
  INSERT INTO search_fts (rowid, entity_kind, entity_id, title, subtitle)
  VALUES (new.rowid * 4 + 1, 'track', new.id, new.title, new.display_artist);
END;

CREATE TRIGGER albums_fts_ai AFTER INSERT ON albums BEGIN
  INSERT INTO search_fts (rowid, entity_kind, entity_id, title, subtitle)
  VALUES (new.rowid * 4 + 2, 'album', new.id, new.title, new.display_artist);
END;

CREATE TRIGGER albums_fts_ad AFTER DELETE ON albums BEGIN
  DELETE FROM search_fts WHERE rowid = old.rowid * 4 + 2;
END;

CREATE TRIGGER albums_fts_au AFTER UPDATE OF title, display_artist ON albums BEGIN
  DELETE FROM search_fts WHERE rowid = old.rowid * 4 + 2;
  INSERT INTO search_fts (rowid, entity_kind, entity_id, title, subtitle)
  VALUES (new.rowid * 4 + 2, 'album', new.id, new.title, new.display_artist);
END;

CREATE TRIGGER artists_fts_ai AFTER INSERT ON artists BEGIN
  INSERT INTO search_fts (rowid, entity_kind, entity_id, title, subtitle)
  VALUES (new.rowid * 4 + 3, 'artist', new.id, new.name, NULL);
END;

CREATE TRIGGER artists_fts_ad AFTER DELETE ON artists BEGIN
  DELETE FROM search_fts WHERE rowid = old.rowid * 4 + 3;
END;

CREATE TRIGGER artists_fts_au AFTER UPDATE OF name ON artists BEGIN
  DELETE FROM search_fts WHERE rowid = old.rowid * 4 + 3;
  INSERT INTO search_fts (rowid, entity_kind, entity_id, title, subtitle)
  VALUES (new.rowid * 4 + 3, 'artist', new.id, new.name, NULL);
END;
