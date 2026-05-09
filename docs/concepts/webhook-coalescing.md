# Webhook coalescing

A single push commonly produces multiple webhook deliveries within
a few seconds (e.g. a force-push followed immediately by a normal
push, or GitHub's occasional duplicate deliveries). Without
coalescing argunix would queue multiple identical evaluations of
the same `(repo, sha)`.

## Mechanism

A short-lived in-memory map of recently-seen `(repo_id, sha)` keys
with a configurable TTL (default `webhook_coalesce_seconds = 5`).

- First webhook for a `(repo_id, sha)` enters the queue normally.
- Subsequent webhooks for the same key within the TTL are
  **silently dropped** — they're treated as duplicates of the
  first.
- After the TTL expires the key is removed; a later webhook for
  the same SHA (e.g. a manual rebuild) starts fresh.

Coalescing happens **before** the cancel-on-new-push path runs, so
a duplicate webhook for the same SHA does not trigger spurious
cancellations of the in-flight evaluation it would otherwise
"replace".

## Why

Webhook deliveries are best-effort and often duplicated by the
forge or by retries. Running the same evaluation twice is harmless
correctness-wise but wastes CPU, builders, and forge API quota.
