-- Per-job Software Bill of Materials.
--
-- argunix generates an exact CycloneDX SBOM for every OCI image it
-- builds, transcribed from the Nix dependency graph (see
-- `argunix-effects::sbom`). The SBOM is attached to the image in the
-- registry as an OCI referrer, and also stored here so the web UI can
-- render it, and so later effect stages (e.g. a devguard upload) can
-- consume it without regenerating.
--
-- `content` is the whole CycloneDX JSON document, stored verbatim as
-- text — deliberately not normalised into columns; consumers parse the
-- JSON. `component_count` is denormalised from it for a cheap
-- "N packages" badge without parsing.
--
-- One SBOM per job (`UNIQUE(job_id)`); a rebuilt job upserts.

CREATE TABLE sboms (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id          INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    format          TEXT NOT NULL,
    content         TEXT NOT NULL,
    component_count INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE(job_id)
);

CREATE INDEX idx_sboms_job ON sboms(job_id);
