-- Failure-mode reason text for jobs that hit a non-build failure path.
-- v1 only writes this for the interruption-cap exceedance case (M13 /
-- design/builders.md Q109): when a job has been interrupted by transport
-- drop or graceful builder shutdown more times than the cap allows, the
-- final flip to `Failure` records the reason here so the UI / forge can
-- distinguish "build failed" from "we gave up retrying transport
-- failures". NULL for normal build failures (Q85 fail-fast).

ALTER TABLE jobs ADD COLUMN failure_reason TEXT;
