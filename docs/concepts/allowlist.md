# PR allowlist and permission gate

argunix does **not** automatically build PRs from arbitrary
authors — running untrusted nix expressions with build access is a
straightforward path to RCE on the coordinator. Instead, every PR
is gated by a two-pronged check; either signal grants build.

## The two checks

1. **Live forge permission lookup.** Query the forge API for the
   PR author's permission level on the target repo. Anyone who can
   already push (committers, maintainers) is allowed.
2. **Static per-repo allowlist.** A list of usernames in the
   argunix YAML config. Any name on this list is allowed
   regardless of forge state.

Either check passing grants the build. Both failing rejects it.

## Fork PRs

For PRs from forks, argunix builds the forge's **merge ref** when
available (the speculative merge against the target branch the
forge computes). If the merge ref isn't there yet, brief retry with
backoff, then fall back to the PR head SHA.

## Failure modes

- **Forge API returns an error:** fall back to the static
  allowlist alone, log a warning. Never auto-elevate beyond the
  allowlist on a forge outage.
- **Forge API returns 401:** mark the forge paused (see
  [forge-pause](forge-pause.md)) and continue with the allowlist
  fallback. Don't reject the build — the operator may still want
  allowlisted users' PRs to build through a token outage.

## Branch matching

For watched non-PR branches (push events), argunix matches the
incoming branch against the repo's `watched_branches` list using
glob patterns (e.g. `release/*`). Exact-match was the v1
behaviour; glob support is the current state.

## Why

Monorepos like nixpkgs have lots of casual contributors whose PRs
should build (committers' PRs, maintainers' PRs); they also have
endless drive-by PRs from accounts the project has never heard of.
The allowlist+permission gate balances "build the people who
already have push" against "don't run a stranger's nix code on
shared infrastructure".
