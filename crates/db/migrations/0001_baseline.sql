-- The baseline schema: every table, index and trigger of a Hearthsound server.
--
-- One file replaces the eighteen migrations of the releases before v0.1.7,
-- because a database from those releases has no supported upgrade path: the
-- server refuses one (see `Database::migrate`) and starts from an empty data
-- directory. This file is the whole schema of that fresh database, and every
-- later change is a forward-only migration after it.
--
-- Tables follow the storage conventions of the data model: STRICT tables, UUIDv7
-- BLOB keys, unix-millisecond INTEGER timestamps, 0/1 INTEGER booleans with CHECK
-- constraints, and JSON held in TEXT columns. Foreign keys are enforced per
-- connection by the database layer.
--
-- Entities are one row per item, whatever its origin. Every artist, album and
-- track the server sees - scanned, uploaded, downloaded, found by a search, named
-- by a discography or suggested by discovery - is a row of `artists`, `albums` or
-- `tracks`, and every provider's identifier for it is a row of `external_ids`.
-- Metadata that came from a provider lives beside the row's own fields
-- (`entity_sources` holds what each instance answered, `entity_images` what it
-- showed); nothing is fetched and discarded. `tracks.origin` and the per-plugin
-- identity columns of earlier schemas are gone: an identifier is a namespace and
-- a value in `external_ids`, and whether a row is in someone's library is
-- `user_library`'s answer, never a column on the row.

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

CREATE TABLE settings (
  key        TEXT    PRIMARY KEY,
  value      TEXT    NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by BLOB    REFERENCES users (id) ON DELETE SET NULL
) STRICT;

CREATE TABLE user_settings (
  user_id BLOB NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  key     TEXT NOT NULL,
  value   TEXT NOT NULL,
  PRIMARY KEY (user_id, key)
) STRICT;

CREATE TABLE library_folders (
  id            BLOB    PRIMARY KEY,
  path          TEXT    NOT NULL,
  name          TEXT    NOT NULL,
  owner_user_id BLOB    REFERENCES users (id) ON DELETE SET NULL,
  watch         INTEGER NOT NULL DEFAULT 1 CHECK (watch IN (0, 1)),
  created_at    INTEGER NOT NULL,
  last_scan_at  INTEGER
, last_scan_error TEXT) STRICT;

CREATE UNIQUE INDEX library_folders_path_uq ON library_folders (path);

CREATE INDEX library_folders_owner_idx ON library_folders (owner_user_id);

CREATE TABLE artists (
  id              BLOB    PRIMARY KEY,
  name            TEXT    NOT NULL,
  sort_name       TEXT    NOT NULL,
  match_key       TEXT    NOT NULL DEFAULT '',
  biography       TEXT,
  links           TEXT    NOT NULL DEFAULT '[]',
  album_count     INTEGER,
  file_tags       TEXT,
  fetched_at      INTEGER,
  page_fetched_at INTEGER,
  deep_indexed_at INTEGER,
  viewed_at       INTEGER,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX artists_sort_name_idx ON artists (sort_name);

CREATE INDEX artists_match_key_idx ON artists (match_key);

CREATE TABLE albums (
  id              BLOB    PRIMARY KEY,
  title           TEXT    NOT NULL,
  sort_title      TEXT    NOT NULL,
  display_artist  TEXT,
  year            INTEGER,
  release_date    TEXT,
  album_type      TEXT,
  edition         TEXT,
  track_count     INTEGER,
  file_tags       TEXT,
  fetched_at      INTEGER,
  page_fetched_at INTEGER,
  deep_indexed_at INTEGER,
  viewed_at       INTEGER,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX albums_sort_title_idx ON albums (sort_title);

CREATE TABLE album_artists (
  album_id  BLOB    NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
  artist_id BLOB    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
  position  INTEGER NOT NULL,
  role      TEXT    NOT NULL,
  PRIMARY KEY (album_id, position)
) STRICT;

CREATE INDEX album_artists_artist_idx ON album_artists (artist_id);

CREATE TABLE tracks (
  id                BLOB    PRIMARY KEY,
  album_id          BLOB    REFERENCES albums (id) ON DELETE SET NULL,
  title             TEXT    NOT NULL,
  sort_title        TEXT    NOT NULL,
  display_artist    TEXT,
  disc_number       INTEGER,
  track_number      INTEGER,
  duration_ms       INTEGER,
  explicit          INTEGER CHECK (explicit IN (0, 1)),
  availability      TEXT    NOT NULL CHECK (availability IN ('local', 'cached', 'acquiring', 'remote', 'unavailable')),
  unavailable_until INTEGER,
  file_tags         TEXT,
  fetched_at        INTEGER,
  deep_indexed_at   INTEGER,
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
) STRICT;

CREATE INDEX tracks_album_idx ON tracks (album_id, disc_number, track_number);

CREATE INDEX tracks_availability_idx ON tracks (availability, unavailable_until);

CREATE TABLE track_artists (
  track_id  BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  artist_id BLOB    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
  position  INTEGER NOT NULL,
  role      TEXT    NOT NULL,
  PRIMARY KEY (track_id, position)
) STRICT;

CREATE INDEX track_artists_artist_idx ON track_artists (artist_id);

CREATE TABLE genres (
  id   BLOB PRIMARY KEY,
  name TEXT NOT NULL
) STRICT;

CREATE UNIQUE INDEX genres_name_uq ON genres (name);

CREATE TABLE track_genres (
  track_id BLOB NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  genre_id BLOB NOT NULL REFERENCES genres (id) ON DELETE CASCADE,
  PRIMARY KEY (track_id, genre_id)
) STRICT;

CREATE INDEX track_genres_genre_idx ON track_genres (genre_id);

CREATE TABLE external_ids (
  namespace   TEXT NOT NULL,
  value       TEXT NOT NULL,
  entity_kind TEXT NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist', 'playlist')),
  entity_id   BLOB NOT NULL,
  PRIMARY KEY (namespace, entity_kind, value)
) STRICT;

CREATE INDEX external_ids_entity_idx ON external_ids (entity_kind, entity_id);

-- One artist holds at most one MusicBrainz artist id: an artist is one
-- MusicBrainz entity, and a second id on one row means a credit named two
-- artists. The library refuses the second id in code; this index is the
-- backstop.
CREATE UNIQUE INDEX external_ids_one_mb_artist ON external_ids (entity_id)
  WHERE entity_kind = 'artist' AND namespace = 'mb-artist';

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

CREATE TABLE track_retention (
  track_id    BLOB    PRIMARY KEY REFERENCES tracks (id) ON DELETE CASCADE,
  pinned      INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
  pin_reasons TEXT    NOT NULL DEFAULT '[]',
  updated_at  INTEGER NOT NULL
) STRICT;

CREATE INDEX track_retention_pinned_idx ON track_retention (pinned);

CREATE TABLE user_library (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist', 'playlist')),
  entity_id   BLOB    NOT NULL,
  origin      TEXT    NOT NULL CHECK (origin IN ('saved', 'upload')),
  added_at    INTEGER NOT NULL,
  PRIMARY KEY (user_id, entity_kind, entity_id, origin)
) STRICT;

CREATE INDEX user_library_entity_idx ON user_library (entity_kind, entity_id);

CREATE TABLE user_feedback (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist')),
  entity_id   BLOB    NOT NULL,
  value       TEXT    NOT NULL CHECK (value IN ('like', 'dislike')),
  at          INTEGER NOT NULL,
  PRIMARY KEY (user_id, entity_kind, entity_id)
) STRICT;

CREATE INDEX user_feedback_entity_idx ON user_feedback (entity_kind, entity_id, value);

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
, detached_at INTEGER) STRICT;

CREATE INDEX playlists_owner_idx ON playlists (owner_user_id);

CREATE INDEX playlists_temporary_idx ON playlists (is_temporary, expires_at);

CREATE TABLE playlist_tracks (
  playlist_id BLOB    NOT NULL REFERENCES playlists (id) ON DELETE CASCADE,
  position    INTEGER NOT NULL,
  track_id    BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  added_at    INTEGER NOT NULL,
  PRIMARY KEY (playlist_id, position)
) STRICT;

CREATE INDEX playlist_tracks_track_idx ON playlist_tracks (track_id);

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
, park_reason TEXT, park_count INTEGER NOT NULL DEFAULT 0, retry_at INTEGER) STRICT;

CREATE INDEX acquisitions_state_idx ON acquisitions (state, priority);

CREATE INDEX acquisitions_track_idx ON acquisitions (track_id);

CREATE INDEX acquisitions_album_idx ON acquisitions (album_id);

CREATE TABLE acquisition_log (
  acquisition_id BLOB    NOT NULL REFERENCES acquisitions (id) ON DELETE CASCADE,
  at             INTEGER NOT NULL,
  state          TEXT    NOT NULL,
  message        TEXT
) STRICT;

CREATE INDEX acquisition_log_acq_idx ON acquisition_log (acquisition_id, at);

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

CREATE TABLE layer_assignments (
  layer       TEXT    NOT NULL CHECK (layer IN ('catalog', 'metadata', 'artwork', 'acquisition', 'verification', 'scrobble', 'discovery', 'lyrics', 'playlist-import')),
  position    INTEGER NOT NULL,
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  PRIMARY KEY (layer, position)
) STRICT;

CREATE INDEX layer_assignments_instance_idx ON layer_assignments (instance_id);

CREATE TABLE plugin_kv (
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  key         TEXT    NOT NULL,
  value       BLOB    NOT NULL,
  expires_at  INTEGER,
  PRIMARY KEY (instance_id, key)
) STRICT;

CREATE INDEX plugin_kv_expiry_idx ON plugin_kv (expires_at);

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

CREATE TABLE bookmarks (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  track_id    BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  position_ms INTEGER NOT NULL,
  comment     TEXT,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (user_id, track_id)
) STRICT;

CREATE TABLE now_playing (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  client      TEXT    NOT NULL,
  track_id    BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  started_at  INTEGER NOT NULL,
  position_ms INTEGER NOT NULL,
  state       TEXT    NOT NULL,
  PRIMARY KEY (user_id, client)
) STRICT;

CREATE TABLE play_queues (
  user_id       BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  client        TEXT    NOT NULL,
  current_index INTEGER NOT NULL,
  position_ms   INTEGER NOT NULL,
  track_ids     TEXT    NOT NULL,
  updated_at    INTEGER NOT NULL,
  PRIMARY KEY (user_id, client)
) STRICT;

CREATE TABLE user_ratings (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist')),
  entity_id   BLOB    NOT NULL,
  rating      INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
  at          INTEGER NOT NULL,
  PRIMARY KEY (user_id, entity_kind, entity_id)
) STRICT;

CREATE TABLE sessions (
  id           BLOB    PRIMARY KEY,
  user_id      BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  token_hash   BLOB    NOT NULL,
  csrf_token   TEXT    NOT NULL,
  created_at   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL,
  user_agent   TEXT
) STRICT;

CREATE UNIQUE INDEX sessions_token_hash_uq ON sessions (token_hash);

CREATE INDEX sessions_user_idx ON sessions (user_id);

CREATE TABLE user_quotas (
  user_id                   BLOB PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
  acquisitions_per_day      INTEGER,
  acquisition_bytes_per_day INTEGER
) STRICT;

CREATE TABLE plugin_upgrades (
  plugin_id       TEXT    PRIMARY KEY REFERENCES plugins (id) ON DELETE CASCADE,
  version         TEXT    NOT NULL,
  api             TEXT    NOT NULL,
  manifest        TEXT    NOT NULL,
  sha256          BLOB    NOT NULL,
  source          TEXT    NOT NULL CHECK (source IN ('official', 'repository', 'sideload', 'bundled')),
  repository_id   BLOB    REFERENCES plugin_repositories (id) ON DELETE SET NULL,
  staged_at       INTEGER NOT NULL
) STRICT;

CREATE TABLE provider_user_configs (
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  config      TEXT,
  linked_at   INTEGER NOT NULL,
  PRIMARY KEY (instance_id, user_id)
) STRICT;

CREATE TABLE scrobble_outbox (
  id              BLOB    PRIMARY KEY,
  user_id         BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  instance_id     BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  kind            TEXT    NOT NULL CHECK (kind IN ('now-playing', 'listen', 'feedback')),
  payload         TEXT    NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER NOT NULL,
  state           TEXT    NOT NULL,
  last_error      TEXT
) STRICT;

CREATE INDEX scrobble_outbox_due_idx ON scrobble_outbox (state, next_attempt_at);

CREATE INDEX scrobble_outbox_user_idx ON scrobble_outbox (user_id, instance_id);

CREATE TABLE source_blocklist (
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  source_key  TEXT    NOT NULL,
  file_key    TEXT    NOT NULL,
  reason      TEXT,
  created_at  INTEGER NOT NULL,
  expires_at  INTEGER,
  PRIMARY KEY (instance_id, source_key, file_key)
) STRICT;

CREATE INDEX source_blocklist_expiry_idx ON source_blocklist (expires_at);

CREATE INDEX user_ratings_entity_idx ON user_ratings (entity_kind, entity_id);

CREATE INDEX acquisitions_retry_idx ON acquisitions (state, retry_at);

CREATE TABLE listen_sessions (
  user_id         BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  client          TEXT    NOT NULL,
  track_id        BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  duration_ms     INTEGER NOT NULL,
  state           TEXT    NOT NULL CHECK (state IN ('playing', 'paused')),
  started_at      INTEGER NOT NULL,
  last_at         INTEGER NOT NULL,
  listened_ms     INTEGER NOT NULL,
  ignore_scrobble INTEGER NOT NULL CHECK (ignore_scrobble IN (0, 1)),
  persisted_at    INTEGER NOT NULL,
  PRIMARY KEY (user_id, client)
) STRICT;

CREATE TABLE library_maintenance (
  name         TEXT    PRIMARY KEY,
  completed_at INTEGER NOT NULL
) STRICT;

CREATE INDEX artists_name_nocase_idx ON artists (name COLLATE NOCASE);


-- Entity identity: merges and provider answers.
--
-- Two rows that turn out to be the same item are merged, and the older row's id
-- survives. A merged-away id keeps resolving through `entity_redirects`, so a
-- client that holds one is never answered with an error, and the server never
-- emits it again.
CREATE TABLE entity_redirects (
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist')),
  old_id      BLOB    NOT NULL,
  new_id      BLOB    NOT NULL,
  merged_at   INTEGER NOT NULL,
  PRIMARY KEY (entity_kind, old_id)
) STRICT;

CREATE INDEX entity_redirects_new_idx ON entity_redirects (entity_kind, new_id);

-- What one provider instance answered about one entity.
--
-- One row per entity, instance and operation, so the last answer of every
-- provider is kept and the fields are recomputed from all of them without
-- calling a provider again. `payload_version` lets a later schema invalidate the
-- stored shapes by bumping the number the reader expects.
CREATE TABLE entity_sources (
  entity_kind     TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist')),
  entity_id       BLOB    NOT NULL,
  instance_id     BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  operation       TEXT    NOT NULL CHECK (operation IN ('catalog', 'catalog-albums', 'catalog-top-tracks',
                                                        'release', 'artist-info', 'artwork', 'similar-artists')),
  status          TEXT    NOT NULL CHECK (status IN ('found', 'missing')),
  payload         TEXT,
  payload_version INTEGER NOT NULL,
  fetched_at      INTEGER NOT NULL,
  PRIMARY KEY (entity_kind, entity_id, instance_id, operation)
) STRICT;

CREATE INDEX entity_sources_instance_idx ON entity_sources (instance_id);

-- Every image the server holds, keyed by its bytes.
--
-- A blob is written once and shared by every entity image row that points at it;
-- the file lives under the artwork directory at `path`. `sweep_orphans` removes a
-- blob no `entity_images` row references.
CREATE TABLE image_blobs (
  hash         BLOB    PRIMARY KEY,
  path         TEXT    NOT NULL,
  content_type TEXT    NOT NULL,
  byte_len     INTEGER NOT NULL,
  created_at   INTEGER NOT NULL
) STRICT;

-- The pictures of one entity, one slot per role and provider.
--
-- `instance_id` is NULL for an image that came from a file's own tags; a provider
-- row keeps the URL it was found at, so a blob that was pruned is fetched again
-- from the same place. `position` orders the renditions a provider offered.
CREATE TABLE entity_images (
  id          BLOB    PRIMARY KEY,
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist')),
  entity_id   BLOB    NOT NULL,
  role        TEXT    NOT NULL CHECK (role IN ('cover', 'portrait')),
  instance_id BLOB    REFERENCES provider_instances (id) ON DELETE CASCADE,
  position    INTEGER NOT NULL,
  url         TEXT,
  width       INTEGER,
  height      INTEGER,
  blob_hash   BLOB    REFERENCES image_blobs (hash),
  fetched_at  INTEGER NOT NULL
) STRICT;

CREATE UNIQUE INDEX entity_images_slot_uq
  ON entity_images (entity_kind, entity_id, role, COALESCE(instance_id, x''), position);
CREATE INDEX entity_images_blob_idx ON entity_images (blob_hash);

-- An artist's most popular tracks, as discovery and the catalog reported them.
--
-- The rows are track ids, so a top track is the same row every other surface
-- serves, and a track that is not on disk yet simply has no file.
CREATE TABLE artist_top_tracks (
  artist_id BLOB    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
  position  INTEGER NOT NULL,
  track_id  BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  PRIMARY KEY (artist_id, position)
) STRICT;

CREATE INDEX artist_top_tracks_track_idx ON artist_top_tracks (track_id);

-- Artists similar to one artist, in the discovery layer's order.
CREATE TABLE artist_similar (
  artist_id  BLOB    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
  position   INTEGER NOT NULL,
  similar_id BLOB    NOT NULL REFERENCES artists (id) ON DELETE CASCADE,
  PRIMARY KEY (artist_id, position)
) STRICT;

CREATE INDEX artist_similar_similar_idx ON artist_similar (similar_id);

-- The genres of one album, resolved from the products' genre lists.
CREATE TABLE album_genres (
  album_id BLOB NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
  genre_id BLOB NOT NULL REFERENCES genres (id) ON DELETE CASCADE,
  PRIMARY KEY (album_id, genre_id)
) STRICT;

-- The entities whose stored provider data is stale or missing.
--
-- The indexer claims one item at a time, deepest work last: `priority` is 0 for
-- an administrator's refresh or a stale view, 1 for a library change, 2 for the
-- periodic sweep and 3 for a scheduled refresh. `depth` decides how far to walk
-- (head, page, deep); a row records the furthest depth asked for and the highest
-- urgency ever asked for.
CREATE TABLE index_queue (
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist')),
  entity_id   BLOB    NOT NULL,
  depth       TEXT    NOT NULL CHECK (depth IN ('head', 'page', 'deep')),
  priority    INTEGER NOT NULL,
  state       TEXT    NOT NULL CHECK (state IN ('queued', 'failed')),
  attempts    INTEGER NOT NULL DEFAULT 0,
  run_at      INTEGER NOT NULL,
  enqueued_at INTEGER NOT NULL,
  last_error  TEXT,
  PRIMARY KEY (entity_kind, entity_id)
) STRICT;

CREATE INDEX index_queue_claim_idx ON index_queue (state, priority, run_at);

-- The provider answers of a search or a discovery request, stored as entity ids.
--
-- An answer is a list of ids that already resolve to rows, never a copy of a
-- provider's records: a cached answer can therefore be served after a merge
-- without naming a row that no longer exists.
CREATE TABLE answer_cache (
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  request_key TEXT    NOT NULL,
  entity_refs TEXT    NOT NULL,
  fetched_at  INTEGER NOT NULL,
  expires_at  INTEGER NOT NULL,
  PRIMARY KEY (instance_id, request_key)
) STRICT;

CREATE INDEX answer_cache_expiry_idx ON answer_cache (expires_at);

-- Rows that name an entity without a foreign key go with it, in the deleting
-- transaction, exactly as 04-data-model.md section 3 requires. One trigger per
-- entity table covers every delete path - the scanner's prune, a folder removal,
-- the catalog's rollback, a merge that drops the loser - so no caller has to
-- remember the tables.
CREATE TRIGGER tracks_polymorphic_ad AFTER DELETE ON tracks BEGIN
  DELETE FROM external_ids   WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM user_library   WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM user_feedback  WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM user_ratings   WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM entity_sources WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM entity_images  WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM index_queue    WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM entity_redirects WHERE entity_kind = 'track' AND new_id = old.id;
END;

CREATE TRIGGER albums_polymorphic_ad AFTER DELETE ON albums BEGIN
  DELETE FROM external_ids   WHERE entity_kind = 'album' AND entity_id = old.id;
  DELETE FROM user_library   WHERE entity_kind = 'album' AND entity_id = old.id;
  DELETE FROM user_feedback  WHERE entity_kind = 'album' AND entity_id = old.id;
  DELETE FROM user_ratings   WHERE entity_kind = 'album' AND entity_id = old.id;
  DELETE FROM entity_sources WHERE entity_kind = 'album' AND entity_id = old.id;
  DELETE FROM entity_images  WHERE entity_kind = 'album' AND entity_id = old.id;
  DELETE FROM index_queue    WHERE entity_kind = 'album' AND entity_id = old.id;
  DELETE FROM entity_redirects WHERE entity_kind = 'album' AND new_id = old.id;
END;

CREATE TRIGGER artists_polymorphic_ad AFTER DELETE ON artists BEGIN
  DELETE FROM external_ids   WHERE entity_kind = 'artist' AND entity_id = old.id;
  DELETE FROM user_library   WHERE entity_kind = 'artist' AND entity_id = old.id;
  DELETE FROM user_feedback  WHERE entity_kind = 'artist' AND entity_id = old.id;
  DELETE FROM user_ratings   WHERE entity_kind = 'artist' AND entity_id = old.id;
  DELETE FROM entity_sources WHERE entity_kind = 'artist' AND entity_id = old.id;
  DELETE FROM entity_images  WHERE entity_kind = 'artist' AND entity_id = old.id;
  DELETE FROM index_queue    WHERE entity_kind = 'artist' AND entity_id = old.id;
  DELETE FROM entity_redirects WHERE entity_kind = 'artist' AND new_id = old.id;
END;
