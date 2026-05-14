# Cache push

argunix signs and pushes every successful build's output closure
to the binary caches listed in `binary_caches`. Users substitute
from those caches instead of rebuilding locally.

## Shape

```yaml
binary_caches:
  - push_url: s3://my-cache?endpoint=https://s3.example.com&region=eu-central-1
    public_url: https://cache.example.com # optional, see below
    public_key: ci.example.com:abc123… # optional, paired with public_url
    signing_key_path: /var/lib/argunix-credentials/cache/secret
  - push_url: file:///srv/argunix-cache
    signing_key_path: /var/lib/argunix-credentials/cache/secret
```

- **`push_url`** is the nix store URI argunix hands to
  `nix copy --to`. Anything `nix copy` accepts works: `s3://`,
  `file://`, `https://<name>.cachix.org`, attic's store URI, …
- **`public_url`** is the URL users put in their `nix.conf`.
  Asymmetric backends (S3 push + CDN read) have it; symmetric
  ones (cachix, attic, file://) leave it unset.
- **`public_key`** is the verbatim `<name>:<base64>` public-key
  line users add to `trusted-public-keys`. Derive it once via
  `nix key convert-secret-to-public < signing-key > public`; both
  fields together let the `/caches` page render copy-pasteable
  substituter snippets without argunix ever reading the secret
  file at request time.
- **`signing_key_path`** is the nix-format secret key (the kind
  `nix key generate-secret` produces). The path is handed to
  `nix copy` via the `secret-key=` query param so the upload is
  signed at write time; clients reject the substitution unless
  the public counterpart is in their `trusted-public-keys`.

## Where the push fires

In the coordinator, after `dispatch_pool_build` pulls the output
closure back from the builder (phase 3 of
[`docs/closure-transfer.puml`](../closure-transfer.puml)).
`argunix-build::push_to_caches` shells out to `nix copy --to
<push_url>?secret-key=<key>` once per cache.

This is deliberately a coordinator-side step rather than a
nix-daemon `post-build-hook`:

| Concern                                       | post-build-hook on every builder | argunix `binary_caches`     |
| --------------------------------------------- | -------------------------------- | --------------------------- |
| Where signing key + creds live                | every builder                    | coordinator only            |
| Per-job push failures                         | journal noise, no DB record      | warning log on the job      |
| Knows which output came from what eval / repo | no                               | yes (carried in argunix DB) |
| Works for non-argunix builds                  | yes                              | no                          |

The coordinator already has every output in its local store
(phase 3 pulls them back), so push is just a `nix copy` to the
configured remote. Builders never need credentials.

## Failure policy

Push failures **never fail the job**. The build succeeded; only
the publish degraded. Argunix logs the failure at `warn` level
with the cache URL and the `nix copy` stderr, and the job stays
`Success`. Retries are deferred to a future eval (or to operator
re-running argunix's push later).

A flaky cache therefore can't poison the build pipeline — at
worst the cache lags behind reality, which is the symmetric
failure mode of every other "publish on success" hook.

## Multiple caches

Every entry in `binary_caches` receives the push, sequentially,
with a five-minute timeout each. Failures are independent: if
cache A times out and cache B succeeds, the operator sees one
warning and the job is still `Success`.

There is currently no per-repo or per-forge routing — every
build pushes to every configured cache. Per-repo routing is a
likely follow-up once a deployment needs it.

## Backend matrix

What works with `push_url`:

| Backend             | `push_url` shape                  | Notes                                                                                          |
| ------------------- | --------------------------------- | ---------------------------------------------------------------------------------------------- |
| S3 / Garage / MinIO | `s3://bucket?endpoint=…&region=…` | AWS creds via `EnvironmentFile` on the argunix systemd unit (`AWS_ACCESS_KEY_ID` / `_SECRET_`) |
| cachix              | `https://<name>.cachix.org`       | Auth token via the same env mechanism as S3                                                    |
| attic               | `<attic-store-uri>`               | Argunix uses the URI as-is; attic CLI auth applies                                             |
| Local directory     | `file:///srv/argunix-cache`       | Useful for "argunix serves its own cache via nginx" topologies                                 |

What does _not_ fit `push_url`:

- **nix-serve** and **harmonia** are read-only HTTP servers that
  expose some host's `/nix/store`. You don't push to them — they
  pull from a store argunix writes to. The natural pattern is
  to put nix-serve on the argunix host and let it serve
  `/nix/store` directly; the argunix `binary_caches` push is
  bypassed.

## Credentials

Argunix doesn't carry storage credentials in `argunix.yaml` — the
file is meant to be reviewable and committable. Cloud creds go in
the systemd unit's environment:

```nix
systemd.services.argunix.serviceConfig.EnvironmentFile =
  "/var/lib/argunix-credentials/cache/s3-credentials";
```

with `s3-credentials` containing the standard AWS env-var
bindings. The nix S3 store reads them automatically; argunix
never touches the file directly.

## Comparing the push and read sides

Read-side caches — what builders themselves pull missing
dependencies from — are owned by **system-wide
`nix.settings.substituters`** on each host, not by argunix's
`binary_caches`. The two sides are intentionally separate:

- Argunix's `binary_caches` is _only about publishing argunix's
  own build outputs_. It runs on the coordinator after a build.
- Read substitution is whatever the host's `nix-daemon` is
  configured with — including the cache argunix pushed to in a
  previous build, plus `cache.nixos.org`, plus anything else.

That separation is also why dropping `substitute: bool` from the
schema simplified the model: argunix doesn't probe its own
configured caches before dispatching builds. The
eval-time `is_cached` flag from
[`nix-eval-jobs --check-cache-status`](https://github.com/nix-community/nix-eval-jobs)
already consults the host's substituters and short-circuits
already-cached jobs before any builder dispatch.

## The /caches page

When `public_url` and `public_key` are both set on an entry, the
`/caches` page renders three copy-pasteable substituter snippets
per cache: one for a flake's `nixConfig`, one for a NixOS module,
one for a plain `nix.conf`. Send users the URL and they pick the
form that matches where they consume the flake.

Entries missing `public_url` or `public_key` still appear (so an
operator sees what's configured) but are tagged "incomplete" with
a hint on which field to set.

## Where it lives

- `argunix-build/src/push.rs` — `push_to_caches` + the
  `nix copy --to` subprocess wrapper.
- `argunix-daemon/src/worker.rs` and `argunix-daemon/src/main.rs`
  — the call site after `BuildStatus::Success`.
- `argunix-config/src/schema.rs` — the `BinaryCache` struct.
- `nix/tests/cache-push.nix` — end-to-end test against a real
  Garage S3 backend + a parallel `file://` cache, demonstrating
  both asymmetric and symmetric config shapes.
