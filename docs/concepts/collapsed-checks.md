# Collapsed check mode

By default argunix posts one forge check per derivation. For
evaluations that produce many jobs, this floods the forge UI and
hits per-PR check-count limits. When the job count exceeds the
configured threshold, argunix collapses to a **single rolling
check** with a markdown summary instead.

## Threshold

- Global default: `100` jobs.
- Per-repo override: `repos[*].check_collapse_threshold`.

If `jobs > threshold`, the collapsed mode is active for that
evaluation; per-job status posts are suppressed.

## Rolling summary

The collapsed check posts a single status whose description and
markdown body are updated as builds complete:

- **Header:** `X passed, Y failed, Z queued` counts that move with
  each completed derivation.
- **Body:** bullet list of failed jobs (capped at ~50 entries with
  a "…and N more" tail), each linking back to the argunix UI for
  the full log.

Updates are **debounced** (a posted summary is only re-sent if at
least 2s elapsed since the last post) so a fast finish on hundreds
of cached jobs doesn't pummel the forge API.

While the evaluation is in progress the description rolls between
"evaluating…", "building…", and the final summary; the initial
"evaluating…" check posted at webhook time is replaced by this
rolling check once the eval completes and the job count is known.

## Why

GitHub caps checks per ref at a few thousand and renders dozens of
checks on a PR very poorly. A nixpkgs-style monorepo evaluation can
produce tens of thousands of jobs; collapsed mode keeps the PR UI
readable without losing per-job detail (still available in the
argunix UI behind the link).
