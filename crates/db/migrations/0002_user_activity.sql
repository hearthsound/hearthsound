-- The per-user activity tables: resume points, the current playback entry, the saved play queue,
-- and ratings.
--
-- They follow the same storage conventions as the initial schema: STRICT tables, UUIDv7 BLOB keys,
-- unix-millisecond INTEGER timestamps, and JSON held in TEXT columns. Every key starts with
-- user_id, so a deleted account takes its activity with it.

-- bookmarks
CREATE TABLE bookmarks (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  track_id    BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  position_ms INTEGER NOT NULL,
  comment     TEXT,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (user_id, track_id)
) STRICT;

-- now_playing
CREATE TABLE now_playing (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  client      TEXT    NOT NULL,
  track_id    BLOB    NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
  started_at  INTEGER NOT NULL,
  position_ms INTEGER NOT NULL,
  state       TEXT    NOT NULL,
  PRIMARY KEY (user_id, client)
) STRICT;

-- play_queues
CREATE TABLE play_queues (
  user_id       BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  client        TEXT    NOT NULL,
  current_index INTEGER NOT NULL,
  position_ms   INTEGER NOT NULL,
  track_ids     TEXT    NOT NULL,
  updated_at    INTEGER NOT NULL,
  PRIMARY KEY (user_id, client)
) STRICT;

-- user_ratings
CREATE TABLE user_ratings (
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  entity_kind TEXT    NOT NULL CHECK (entity_kind IN ('track', 'album', 'artist')),
  entity_id   BLOB    NOT NULL,
  rating      INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
  at          INTEGER NOT NULL,
  PRIMARY KEY (user_id, entity_kind, entity_id)
) STRICT;
