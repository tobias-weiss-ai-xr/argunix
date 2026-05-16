-- Post-build effect runs.
--
-- An *effect* is an authenticated, impure thing argunix does with a
-- successful build's outputs: pushing a docker image to an external
-- registry, pushing the closure to a binary cache, (later) deploying a
-- NixOS system. Each attempt against each target gets one row here, so
-- the question "did opencode actually get pushed to ghcr, and when, and
-- why did it fail" is answerable from the database rather than only
-- from daemon logs.
--
-- The runner writes a row in `running` state before invoking the
-- effect, then updates it to a terminal status:
--
--   - kind   : effect family — 'registry-push' | 'cache-push'
--   - target : the named thing acted on — a `registries` catalog name,
--              or a binary cache push URL
--   - status : 'running' | 'success' | 'failure' | 'skipped'
--   - detail : one-line human summary on success/skip, error on failure
--
-- A `running` row that never reached a terminal state is the signature
-- of a daemon that died mid-effect.

CREATE TABLE effect_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id      INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,
    target      TEXT NOT NULL,
    status      TEXT NOT NULL,
    detail      TEXT,
    started_at  TEXT NOT NULL,
    finished_at TEXT
);

CREATE INDEX idx_effect_runs_job ON effect_runs(job_id);
