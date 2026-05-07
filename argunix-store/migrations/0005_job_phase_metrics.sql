-- Per-job phase accounting for pool-dispatched builds (M16).
--
-- Each pool dispatch goes through three phases:
--   1. PUSH: daemon → builder, the drv's input closure (`nix copy --to`).
--   2. BUILD: agent runs `nix-store --realise` on the builder.
--   3. PULL: builder → daemon, the output closure (`nix copy --from`).
--
-- We persist the bytes flowing through our russh tunnel in each
-- direction plus the wall-clock duration of each phase so the UI can
-- render "where does the time / bandwidth go" without N+1 lookups.
--
-- All columns NULL for jobs that were never dispatched (still queued)
-- or built locally (no remote builder); only pool-dispatched terminal
-- rows fill them in.
ALTER TABLE jobs ADD COLUMN push_bytes INTEGER;
ALTER TABLE jobs ADD COLUMN push_ms    INTEGER;
ALTER TABLE jobs ADD COLUMN build_ms   INTEGER;
ALTER TABLE jobs ADD COLUMN pull_bytes INTEGER;
ALTER TABLE jobs ADD COLUMN pull_ms    INTEGER;
