-- The album's edition label: what a release calls itself beyond its title, so
-- two releases of one title read apart in a grid ("Deluxe Edition"). The
-- scanner derives it from the release disambiguation tag or from the album's
-- folder, and the `library.editions` pass fills it in for the rows that existed
-- before the column. Empty when the album states no edition.
ALTER TABLE albums ADD COLUMN edition TEXT;
