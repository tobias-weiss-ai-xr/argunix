-- Per-eval timing split: stamp the moment the evaluator finished
-- and the worker entered the build phase, so the UI can break
-- `total = eval_time + build_time` apart on the per-eval page.
-- NULL on rows that finished before this column existed, on rows
-- that never reached `Building` (eval-failed, cancelled mid-eval),
-- and on rows currently mid-evaluation.
ALTER TABLE evaluations ADD COLUMN building_started_at TEXT;
