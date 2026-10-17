-- The staged plugin upgrades table.
--
-- An upgrade that asks for permissions the installed version does not hold waits
-- here until an administrator approves it. The installed version keeps serving
-- while the row exists, and the row follows its plugin: deleting the plugin takes
-- the staged upgrade with it.

-- plugin_upgrades
CREATE TABLE plugin_upgrades (
  plugin_id       TEXT    PRIMARY KEY REFERENCES plugins (id) ON DELETE CASCADE,
  version         TEXT    NOT NULL,
  api             TEXT    NOT NULL,
  manifest        TEXT    NOT NULL,
  sha256          BLOB    NOT NULL,
  source          TEXT    NOT NULL CHECK (source IN ('official', 'repository', 'sideload', 'bundled')),
  repository_id   BLOB    REFERENCES plugin_repositories (id) ON DELETE SET NULL,
  staged_at       INTEGER NOT NULL
) STRICT;
