# Forge pause on auth failure

When a forge API call returns `401 Unauthorized`, argunix flips that
forge into a **paused** state and stops dispatching work to it
until a successful auth attempt clears the flag.

## Why

A 401 means the configured token is broken — revoked, expired,
rotated out from under us, or scoped wrong. Hammering a broken
token does nothing useful and can rate-limit the operator's whole
account. Pausing surfaces the problem loudly (in the daemon log
and on the status page) instead of silently posting failures.

## Mechanism

`PausedForges` is an in-memory set keyed by forge name.

- **Setting:** the permission-query path (used by the PR allowlist
  gate) marks the forge paused on a 401 from the forge API. Other
  call sites that already produce a clear human error (token-load
  failures at startup, push-time `post_check`) also feed in.
- **Clearing:** any successful permission lookup clears the entry.
  After the operator drops a new token at the configured
  `token_path` and runs `argunixctl reload`, the next webhook
  exercises the permission API and unpauses.

Webhook ingestion still accepts events from a paused forge — they
queue normally — but `post_check` calls and per-job status updates
are skipped while paused, so we don't generate a stream of
stale-status posts on the forge.

The webhook payload from a paused forge is still validated (HMAC,
allowlist) and the build is still scheduled; only the _outbound_
status posts are suppressed.
