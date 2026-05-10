# Scheduling

argunix's scheduler decides _when_ to dispatch each derivation
build. Two design choices set the shape:

1. **Evaluations run serially**, builds run **as parallel as possible
   across evaluations**.
2. **Within one evaluation**, top-level Jobs that depend on each
   other are gated — Job B only dispatches once Job A has finished —
   so two builders never independently rebuild the same
   shared-by-the-flake derivation.

## Layers

```
   eval task                         build dispatch
 ┌─────────────────────┐    ┌──────────────────────────────────┐
 │ pull EvalId from rx │    │ DagStrategy (per eval)           │
 │   clone repo        │    │   pending → ready → running →    │
 │   nix-eval-jobs     │ ─→ │   terminal                       │
 │   persist Jobs      │    │   gated by top-level→top-level   │
 │   walk_closures     │    │   deps                           │
 │   spawn build phase │    │ JoinSet of build_one futures     │
 └─────────────────────┘    │   gated by global_build_sem      │
   ↓ returns immediately    └──────────────────────────────────┘
   pulls next EvalId             ↑
                                 │ N+1's build phase joins
                                 │ the same global pool
```

The eval task pulls one `EvalId` at a time, runs clone + eval +
persist, then `tokio::spawn`s the build phase as a detached task and
returns. Eval N+1's clone+eval starts immediately, concurrently with
N's still-running builds. The global cap (`build_concurrency`,
default 4) is enforced by a single `Semaphore` shared across all
evals' build tasks — _not_ per-eval.

## The DAG: top-level → top-level gating

After persisting the eval's Jobs, the worker calls
`nix derivation show --recursive` over every Job's drv path
(`argunix_eval::walk_closures`). For each top-level Job, the worker
filters that closure to drvs that are _also_ top-level Jobs in this
same eval. That filtered set becomes the Step's `input_drvs` in the
`DagStrategy` graph.

Internal Steps — drvs in the closure that aren't top-level Jobs
(bash, stdenv, glibc, …) — are _not_ enqueued. The strategy treats
them as external and the builders substitute them as today
(cache.nixos.org or your post-build cache, per [gc-roots](gc-roots.md)).

### Example: the original problem

A flake exposes two top-level packages:

```nix
{
  packages.x86_64-linux = {
    a = pkgs.writeText "a" "hello";
    b = pkgs.runCommand "b" {} "cat ${self.packages.x86_64-linux.a} > $out";
  };
}
```

`B`'s drv has `A`'s drv in its `inputDrvs`.

- **Without gating** (the pre-refactor behaviour): both A and B
  dispatch in parallel. B's builder pulls B's drv-closure, sees A
  is needed but hasn't been built yet, and rebuilds A _itself_.
  Two builders end up doing A's work — wasted CPU, wasted
  closure-transfer bandwidth.
- **With gating**: A is enqueued with `input_drvs: []`, B with
  `input_drvs: [a-drv-path]`. The strategy promotes A to Ready
  immediately; B stays Pending. A finishes → strategy decrements
  B's `deps_unfinished` → B becomes Ready and dispatches. By then
  A's output is in the post-build cache (or local store of the
  same builder, if scheduled there), so B's build substitutes A
  rather than rebuilding.

### Diamonds, chains, and shared internal deps

The graph handles arbitrary shapes:

- **Chain A → B → C**: dispatched in that order; if A fails, B
  and C cascade-skip without ever dispatching.
- **Diamond A,B → C**: A and B run in parallel; C waits for both.
- **Shared internal step Z used by top-level X and Y**: Z is
  _not_ in the graph (it's not a top-level Job). X and Y both
  dispatch in parallel; whichever builder lands first builds Z,
  the other pulls Z's output via cache substitution. Internal
  closure dedup is **not** done at the scheduler — it's deferred
  to the cache layer.

## How this differs from Hydra

[Hydra](https://github.com/NixOS/hydra)'s queue runner explodes the
_full transitive closure_ of every top-level Job into per-`.drv`
**Steps** in the database (`BuildSteps` table) and dispatches at
`.drv` granularity. A single `glibc.drv` shared between two
top-level packages is one Step in Hydra's queue runner, dispatched
exactly once across the whole system.

argunix doesn't do this:

|                                          | Hydra                                                   | argunix                                         |
| ---------------------------------------- | ------------------------------------------------------- | ----------------------------------------------- |
| Dispatch granularity                     | per-`.drv` (closure exploded)                           | per top-level Job                               |
| Source of dep edges                      | parse `.drv` files at queue-runner load                 | `nix derivation show --recursive` once per eval |
| Shared internal-`.drv` dedup             | yes, global via in-memory `HashMap<drvPath, Arc<Step>>` | no — deferred to substituters                   |
| DB rows per build                        | 1 + ~200 sub-steps                                      | 1 (top-level Job only)                          |
| Cross-eval dedup                         | yes, via the same global Step map                       | no — each eval's strategy is independent        |
| Dependency gating between top-level Jobs | yes (by virtue of step-level dispatch)                  | yes (this doc)                                  |

The argunix design accepts the substituter as the dedup mechanism
for internal closure: nixpkgs-internal drvs are reachable on
`cache.nixos.org`, and the operator's post-build push cache covers
their internal libs. The cost is one rebuild of an internal drv on
every fresh builder that hasn't seen it yet; the gain is a much
smaller scheduler with no per-`.drv` database rows.

The trade also means argunix's DAG only _needs_ `nix derivation
show --recursive` to identify top-level→top-level edges, not to
scaffold a full step graph. The walker call is one subprocess per
eval, regardless of closure size, where Hydra's queue runner reads
every transitive `.drv` file off disk on every load.

## Why this shape

- **Eval phase serial** keeps `git clone` + `nix-eval-jobs` from
  competing for the same workdir/disk and lets the operator
  reason about config-reload boundaries (each eval snapshots
  `ConfigSnapshot` at its start; concurrent evals would
  complicate which snapshot wins).
- **Build phase concurrent across evals** is the user-visible
  parallelism win: a hot branch with multiple pushes per minute
  doesn't serialize behind whichever build is slowest.
- **Per-eval, top-level-only DAG** is the simplest gating that
  fixes the visible duplicate-work case (top-level A → top-level
  B). Going finer (Hydra-style internal-step dispatch) would
  require per-`.drv` DB rows, an internal-step builder, and
  per-`.drv` log paths — bigger surface area, deferred until the
  internal-closure-rebuild cost actually shows up in production.

## Where it lives

- `argunix-sched/src/{lib,wfq,flat,dag}.rs` — the strategy trait,
  flat WFQ implementation, and the DAG-aware variant. Both share
  a generic WFQ core for cross-repo fairness.
- `argunix-eval/src/closure.rs` — `walk_closures` shells out to
  `nix derivation show --recursive` and parses the result.
- `argunix-daemon/src/worker.rs` — `run_build_phase` constructs
  the per-eval `DagStrategy`, enqueues each top-level Job with
  its filtered `input_drvs`, drives the dispatch loop, and
  routes `CompletionEffects::cascaded_skips` through
  `handle_cascade_skip`.
- `argunix-daemon/src/main.rs` — `WorkerContext.global_build_sem`
  is the single semaphore shared across all evals' build phases.

## Future work

The trait's `Dispatched` already carries `head_job: Option<JobId>`,
where `None` denotes an internal Step. Wiring the daemon side
(a `process_internal_step` builder that mirrors `build_one` minus
DB row updates, and per-internal-Step log paths) would unlock
Hydra-style closure dedup. The strategy is ready for it; the
daemon is not. No commitment to ship.
