-- Forge web URL on the repo. Populated from `repository.html_url`
-- (GitHub / Forgejo) or `repository.web_url` (GitLab) on every webhook
-- payload. Used by the UI to link rows directly to the forge — both
-- the project page and per-eval PR / branch / commit links derive
-- from this. NULL until the first matching webhook lands; the UI
-- falls back to a forge-level URL constructed from the YAML config.
ALTER TABLE repos ADD COLUMN web_url TEXT;
