-- Fingerprint verification follows the verification layer: an instance in the
-- layer turns it on and an empty layer turns it off, so the setting that
-- decided it no longer exists. The settings loader would only skip a stored
-- value for an unregistered key, so the row goes with the definition.
DELETE FROM settings WHERE key = 'acquisition.verify.fingerprint';
