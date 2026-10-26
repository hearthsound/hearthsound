-- The open playback session of each user and client, so a restart mid-track
-- keeps the listening time the client already reported.
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
