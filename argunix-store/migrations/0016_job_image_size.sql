-- Built OCI-image size.
--
-- The on-disk size, in bytes, of the `oci-archive` / `docker-archive`
-- an image job produced. Recorded post-build by the coordinator — the
-- output closure has been pulled back by then — so the job page can
-- show how small a Nix distroless image is, right next to build time.
--
-- NULL for non-image jobs, for failed jobs, and for rows that pre-date
-- this column.
ALTER TABLE jobs ADD COLUMN image_size_bytes INTEGER;
