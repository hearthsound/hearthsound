-- The one-time library passes that have completed, one row per pass. The
-- `reidentity-1` row means the identity rule has been applied to the rows that
-- existed before it shipped; a pass that a start could not finish leaves no row
-- and runs again on the next start.
CREATE TABLE library_maintenance (
  name         TEXT    PRIMARY KEY,
  completed_at INTEGER NOT NULL
) STRICT;
