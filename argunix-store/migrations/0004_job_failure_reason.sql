-- Failure-mode reason text for jobs that hit a non-build failure path.
-- Currently only written for the interruption-cap exceedance case
-- (dynamic builder pool): when a job has been interrupted by transport
-- drop or graceful builder shutdown more times than the cap allows,
-- the final flip to `Failure` records the reason here so the UI /
-- forge can distinguish "build failed" from "we gave up retrying
-- transport failures". NULL for normal build failures.

ALTER TABLE jobs ADD COLUMN failure_reason TEXT;
