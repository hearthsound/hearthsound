-- Per-user provider configuration and account linkage.
--
-- A listener links their account on a scrobble or discovery service, and the
-- configuration the plugin needs for every later call is
-- stored here. `config` holds the sealed envelope of the account name and the
-- plugin's user configuration, so a token the listener entered never sits in
-- plaintext in the database.

CREATE TABLE provider_user_configs (
  instance_id BLOB    NOT NULL REFERENCES provider_instances (id) ON DELETE CASCADE,
  user_id     BLOB    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  config      TEXT,
  linked_at   INTEGER NOT NULL,
  PRIMARY KEY (instance_id, user_id)
) STRICT;
