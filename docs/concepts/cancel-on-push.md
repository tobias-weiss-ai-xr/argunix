# Cancel-on-new-push

When a PR branch receives a new push, any in-flight evaluation or
build for the _previous_ SHA on that branch is cancelled — the new
push supersedes it, and finishing the old SHA would only waste
builders and post stale forge statuses.

## Scope

- **PR branches:** cancel previous in-flight work for the same
  `(repo, branch)`.
- **Watched non-PR branches** (`main`, release branches): do **not**
  cancel. Those builds are historical; users may still want the
  result on the old SHA even after a newer commit lands.

## Mechanism

Each in-flight evaluation owns a `CancellationToken`. The webhook
path looks up the per-eval token by `(repo, branch)` when a new
push arrives and triggers it. Workers cooperate by checking the
token at safe points:

- between clone, eval, and build phases
- inside the per-job build loop after each derivation completes
- mid-build, racing the running `nix-store --realise` against a
  cancel future — on a cancel-wins outcome we drop the build future
  and kill the child process, the nix daemon aborts, and no GC root
  is recorded.

Cancellation is **cooperative**: the worker decides where it's safe
to stop. If a build finished successfully just as cancel arrived,
the success is honored — we don't retroactively fail a green build.

## Why

Without this, a hot branch with rapid pushes would queue up parallel
evaluations on stale commits, occupy builders, and post a churn of
forge statuses for SHAs nobody cares about.
