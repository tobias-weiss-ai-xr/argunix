-- Per-build docker-image registry index.
--
-- One row is inserted whenever a successful build's JobSpec was flagged
-- with `meta.docker-image == true` and the conversion to OCI blobs
-- succeeded. The argunix-registry HTTP layer reads this table to resolve
-- `<image_name>:<tag>` requests:
--
--   - tag = `<branch>`            -> latest row matching git_ref
--   - tag = `latest`              -> latest row matching the repo's default branch
--   - tag = `sha-<short>`         -> exact sha prefix match
--   - tag = `sha256:<hex>`        -> exact manifest_digest match
--
-- For multi-arch image-index assembly the lookup also keys on system,
-- so a single (image_name, git_ref) tuple may have one row per system.

CREATE TABLE docker_images (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    eval_id INTEGER NOT NULL REFERENCES evaluations(id) ON DELETE CASCADE,
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    image_name TEXT NOT NULL,        -- "<forge>/<owner>/<repo>/<attr-name>"
    system TEXT NOT NULL,            -- "x86_64-linux" etc.
    git_ref TEXT NOT NULL,           -- "refs/heads/main"
    sha TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,   -- "sha256:abc...", computed over manifest bytes
    manifest_path TEXT NOT NULL,     -- absolute path to manifest.json on disk
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_docker_images_lookup
    ON docker_images(image_name, git_ref, created_at DESC);
CREATE INDEX idx_docker_images_sha
    ON docker_images(image_name, sha);
CREATE INDEX idx_docker_images_digest
    ON docker_images(manifest_digest);
