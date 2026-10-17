-- The web-session and acquisition-quota tables.
--
-- They follow the same storage conventions as the rest of the schema: STRICT
-- tables, UUIDv7 BLOB keys, unix-millisecond INTEGER timestamps, and a cascade
-- from `users` so deleting an account takes its sessions and its quota row with
-- it.

-- sessions
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

-- user_quotas
CREATE TABLE user_quotas (
  user_id                   BLOB PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
  acquisitions_per_day      INTEGER,
  acquisition_bytes_per_day INTEGER
) STRICT;
