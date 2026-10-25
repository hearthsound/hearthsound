-- Rows that name a track without a foreign key. They go with the track, in the
-- deleting transaction, exactly as 04-data-model.md section 3 requires. The
-- trigger covers every delete path - the scanner's prune, a folder removal, the
-- catalog's rollback - so no caller has to remember the four tables.
CREATE INDEX user_ratings_entity_idx ON user_ratings (entity_kind, entity_id);

CREATE TRIGGER tracks_polymorphic_ad AFTER DELETE ON tracks BEGIN
  DELETE FROM external_ids  WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM user_library  WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM user_feedback WHERE entity_kind = 'track' AND entity_id = old.id;
  DELETE FROM user_ratings  WHERE entity_kind = 'track' AND entity_id = old.id;
END;
