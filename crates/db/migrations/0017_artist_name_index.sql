-- The identity rule looks artists up by their name with NOCASE, which no index
-- covered: `artists_sort_name_idx` sorts by the sort key, not by the name.
CREATE INDEX artists_name_nocase_idx ON artists (name COLLATE NOCASE);
