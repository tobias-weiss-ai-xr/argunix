# Concepts

Short notes on argunix's recurring design concepts — the things
referenced repeatedly from comments throughout the codebase. Each
note describes the _what_ and the _why_; the _how_ lives in the
code.

| Concept                                                                           | Where it lives in code                                         |
| --------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| [allowlist](allowlist.md) — PR allowlist and permission gate                      | `argunix-web/src/policy.rs`, `argunix-forge/src/permission.rs` |
| [cancel-on-push](cancel-on-push.md) — superseded evals are cancelled              | `argunix-web/src/cancel.rs`, `argunix-daemon/src/worker.rs`    |
| [collapsed-checks](collapsed-checks.md) — single rolling check for many-job evals | `argunix-daemon/src/worker.rs`                                 |
| [forge-pause](forge-pause.md) — auth failures pause the forge                     | `argunix-web/src/pause.rs`, `argunix-web/src/policy.rs`        |
| [gc-roots](gc-roots.md) — store retention via GC roots                            | `argunix-build/src/gc_root.rs`, `argunix-daemon/src/gc.rs`     |
| [scheduling](scheduling.md) — eval serial, builds parallel + top-level DAG gating | `argunix-sched/`, `argunix-daemon/src/worker.rs`               |
| [webhook-coalescing](webhook-coalescing.md) — drop duplicate `(repo, sha)` events | `argunix-web/src/coalesce.rs`                                  |
