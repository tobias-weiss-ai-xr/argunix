-- Per-eval failure detail: when an evaluation transitions to
-- `EvaluationFailed`, the worker records the (often multi-line)
-- error string here so the UI can show *why* it failed instead of
-- just the bare status. NULL on every other terminal status, and
-- on rows that finished before this column existed.
--
-- Mirrors `jobs.failure_reason` (migration 0004). Length is
-- unbounded; if we ever want to cap it we can do that at write
-- time in the worker.
ALTER TABLE evaluations ADD COLUMN failure_reason TEXT;
