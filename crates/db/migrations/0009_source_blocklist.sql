-- The quarantine list for sources whose files failed verification.
--
-- A source is identified by the instance that offered it and its own key, and a
-- file by the name the candidate reported. The pair is kept until `expires_at`
-- passes, so a source that handed back the wrong recording is not offered again
-- for a while. `reason` carries what the check saw, for the administrator's
-- view of the acquisition log.

-- source_blocklist
CREATE TABLE source_blocklist (
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  source_key  TEXT    NOT NULL,
  file_key    TEXT    NOT NULL,
  reason      TEXT,
  created_at  INTEGER NOT NULL,
  expires_at  INTEGER,
  PRIMARY KEY (instance_id, source_key, file_key)
) STRICT;

CREATE INDEX source_blocklist_expiry_idx ON source_blocklist (expires_at);
