-- Auto-installed webhooks (Q39bis): argunix generates the secret and
-- pushes it to the forge alongside hook creation. Both columns are
-- NULL until the first ensure-webhook pass populates them; subsequent
-- boots read from sqlite instead of the filesystem.
ALTER TABLE repos ADD COLUMN webhook_secret BLOB;
ALTER TABLE repos ADD COLUMN webhook_hook_id TEXT;
