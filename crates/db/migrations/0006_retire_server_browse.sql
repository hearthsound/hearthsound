-- The server browse view is an administrator's now, so the setting that
-- decided its content no longer exists. The settings loader would only skip a
-- stored value for an unregistered key, so the row goes with the definition.
DELETE FROM settings WHERE key = 'privacy.server_browse';
