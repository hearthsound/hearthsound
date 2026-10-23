-- The durable scrobble outbox.
--
-- Every listen, now-playing report and feedback change a linked listener
-- produces is written here and delivered to the service later, so a service
-- that is down, slow or rate limiting loses nothing. `payload` is the JSON body
-- for that service, `next_attempt_at` is when the row becomes due again after a
-- failure, and `state` is `pending` until the row is delivered or given up on.

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
