-- Persist enough per-job metadata to reconstruct a synthetic flake that
-- substitutes already-cached outputs without re-evaluating the upstream
-- repo.
--
-- `main_program` is `meta.mainProgram` from nix-eval-jobs (the
-- conventional name of the executable inside `<output>/bin/`). Used to
-- build `apps.<system>.<attr>.program = "<bin_output>/bin/<main_program>"`
-- for `nix run`.
--
-- `outputs_json` is the JSON-encoded output-name → store-path map (e.g.
-- `{"out":"/nix/store/...","dev":"/nix/store/...-dev"}`). Used to pick
-- the right output path when emitting the synthetic flake (prefer `bin`
-- if present, otherwise `out`). NULL on rows that pre-date this column
-- — those won't appear in synthetic flakes, only fresh evals will.
ALTER TABLE jobs ADD COLUMN main_program TEXT;
ALTER TABLE jobs ADD COLUMN outputs_json TEXT;
