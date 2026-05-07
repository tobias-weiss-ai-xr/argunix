-- Dynamic builder pool (M13). Builders dial argunix over SSH (russh server)
-- and self-describe their capabilities; argunix never declares them.
--
-- Auth model is TOFU:
--   * first connect uses a shared enrollment token (password method),
--     after which argunix stores `(name, pubkey, capabilities)` here;
--   * subsequent connects use pubkey auth against this row.
--
-- Capabilities (`systems`, `features`) are JSON arrays of strings; refreshed
-- on every reconnect's `hello` message and overwritten in place. The row is
-- the latest snapshot, not history.
--
-- See design/builders.md.

CREATE TABLE builders (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL UNIQUE,
    pubkey       BLOB NOT NULL,           -- ed25519 raw, 32 bytes
    systems      TEXT NOT NULL,           -- JSON array of strings
    features     TEXT NOT NULL,           -- JSON array of strings
    max_jobs     INTEGER NOT NULL,
    nix_version  TEXT NOT NULL,
    enrolled_at  TEXT NOT NULL,           -- RFC 3339 UTC
    last_seen    TEXT NOT NULL,           -- RFC 3339 UTC
    revoked_at   TEXT                     -- NULL = active
);

CREATE INDEX idx_builders_pubkey_active ON builders(pubkey) WHERE revoked_at IS NULL;

-- Track which builder ran each job (for anti-affinity on re-queue) and how
-- many times the job has been interrupted by transport failure / graceful
-- shutdown. Capped at 3 in application code (see design/builders.md Q109);
-- on the 4th interruption, the job flips to `Failure` with a reason.
ALTER TABLE jobs ADD COLUMN builder_id INTEGER REFERENCES builders(id);
ALTER TABLE jobs ADD COLUMN interrupt_count INTEGER NOT NULL DEFAULT 0;
