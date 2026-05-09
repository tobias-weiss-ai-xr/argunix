-- README badge semantics: `/badge/<forge>/<slug>.svg` should reflect the
-- *default branch* of the repo, not "the most recent terminal eval on
-- any branch" (which previously let a failed PR turn the README badge
-- red while main was green). All three forges expose
-- `repository.default_branch` / `project.default_branch` on every
-- webhook payload — we now persist it on the repo row so the badge
-- endpoint can filter by it. NULL until the first matching webhook
-- lands; the badge endpoint falls back to "any branch" then.
ALTER TABLE repos ADD COLUMN default_branch TEXT;
