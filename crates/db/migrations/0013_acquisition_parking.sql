-- A parked acquisition waits for an acquisition source instead of failing.
--
-- `park_reason` is 'no-source' when the acquisition layer serves no instance
-- with an acquisition provider, and 'source-unavailable' when an instance is
-- configured but did not answer. `retry_at` is the unix-millisecond time the
-- scheduler may claim the row again; NULL means "as soon as a source exists".
-- `park_count` counts consecutive parks, is reset by a real attempt, and sets
-- the backoff between retries.
ALTER TABLE acquisitions ADD COLUMN park_reason TEXT;
ALTER TABLE acquisitions ADD COLUMN park_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE acquisitions ADD COLUMN retry_at INTEGER;

CREATE INDEX acquisitions_retry_idx ON acquisitions (state, retry_at);
