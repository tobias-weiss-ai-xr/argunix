# GC roots and store retention

argunix delegates store expiration to NixOS's automatic GC. We
hold **GC roots** for the outputs we want to keep; everything not
rooted is fair game for `nix-collect-garbage`.

## Layout

One symlink per built output, under

```
/nix/var/nix/gcroots/argunix/<repo>/<eval>/<job>
```

This makes it cheap to enumerate roots per repo or per evaluation,
and means retention purges can drop entire subtrees with `rm -rf`.

## Policy

- **Successful builds get a root.** The output stays alive until
  the retention purge or operator action removes it.
- **Failed builds get no root.** Only the captured log is kept —
  the partial outputs (if any) are reachable for the few seconds
  before the next GC and then gone.
- **Cancelled builds get no root.** Killing the child of
  `nix-store --realise` aborts the nix daemon's build; no partial
  outputs are committed.

## Retention purge

A background ticker (default: hourly) runs the retention pickers
against the database, finds **terminal** evaluations to drop
(success/failed; never running/queued), removes their GC root
subtrees, and deletes the corresponding rows. Defaults are
keep-everything; operators set `retention.max_size_gb` and/or a
max age to bound disk use.

`max_size_gb` is the **store closure** budget — the union NAR size
of every store path pinned by an argunix gcroot, computed by
`nix-store --query --requisites` followed by `nix-store --query
--size`. This is the variable that actually shows up in `df`. When
the budget is exceeded the size pass drops oldest-first, then
re-measures; this catches closures shared between evals (so it
doesn't over-evict on a single batch).

The size pass cleans up gcroot symlinks but does not invoke
`nix-store --gc`. Nix's automatic GC (or the operator's
`nix-collect-garbage`) does the actual store reclaim — usually
triggered by `nix.settings.min-free` on NixOS.
