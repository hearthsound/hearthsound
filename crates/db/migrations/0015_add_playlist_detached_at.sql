-- Set on a generated playlist the user has edited: it keeps the columns that
-- identify its service slot so the next sync recognises it and leaves it alone,
-- but it is the user's playlist from then on.
ALTER TABLE playlists ADD COLUMN detached_at INTEGER;
