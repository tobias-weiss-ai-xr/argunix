-- Strip the `refs/heads/` prefix from existing push-triggered eval
-- rows. New pushes are normalized at ingest in webhook.rs; this
-- backfill harmonises older rows so the UI / branch-link builder can
-- treat all push refs uniformly. PR-triggered rows (synthetic
-- `refs/pull/<n>/head:<branch>` shape) are untouched.
UPDATE evaluations
SET git_ref = SUBSTR(git_ref, LENGTH('refs/heads/') + 1)
WHERE git_ref LIKE 'refs/heads/%';
