-- Grant the stored manifest's permissions to plugins installed before this release.
--
-- Earlier releases recorded a plugin's manifest but left `approved_permissions` NULL
-- until an administrator approved it, and kept the row disabled in the meantime.
-- Installing is now the administrator's consent, so every such row is granted exactly
-- what its stored manifest requests, or an empty set when it requests nothing, and is
-- enabled. A row that already holds a grant is left alone.

UPDATE plugins SET approved_permissions = COALESCE(json_extract(manifest, '$.permissions'), '{}'), enabled = 1 WHERE approved_permissions IS NULL;
