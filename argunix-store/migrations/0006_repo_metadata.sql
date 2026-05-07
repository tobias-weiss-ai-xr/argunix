-- UI surfaces (Q-ui-todod): show forge-supplied repo name + description
-- on the /repos and per-repo pages, and link evals to their PR on the
-- forge. All values are `NULL` until the first matching webhook lands;
-- the UI falls back to the slug / drops the field when missing.
ALTER TABLE repos ADD COLUMN name TEXT;
ALTER TABLE repos ADD COLUMN description TEXT;

ALTER TABLE evaluations ADD COLUMN pr_number INTEGER;
