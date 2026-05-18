[![argunix](https://argunix.nix-consulting.net/badge/codeberg/tfc/argunix/main.svg)](https://argunix.nix-consulting.net/r/codeberg/tfc/argunix)

# argunix

A declarative, Nix-native CI for self-hosters. argunix watches your
repositories on GitHub, GitLab, and Forgejo/Gitea/Codeberg, evaluates
each push and PR as a Nix flake, and builds every `packages.<system>`,
`checks.<system>`, `devShells.<system>`, and `nixosConfigurations.<name>`
attribute it finds — then posts the results back to the forge as
commit statuses.

It is built for the individual developer or the small team who wants
a single, operator-configurable CI box that they can stand up with a
NixOS module and forget about. There is no web admin UI to click
through to create projects, no Postgres to babysit, no separate
worker manager. One Rust daemon, one SQLite database, one YAML
config (or the equivalent NixOS module options), and any number of
remote builders enrolled over SSH.

<!-- TODO screenshot: argunix status page, showing the builder grid (online/draining/offline rows) and the live "running / queued" job tickers. -->

## What it does well

- **Multi-forge from one daemon.** GitHub, GitHub Enterprise Server,
  GitLab (.com and self-hosted), and the Forgejo family
  (Codeberg, Gitea, self-hosted Forgejo) are all first-class. Each
  configured forge gets its webhook auto-installed and re-patched
  if drift is detected, so the operator never logs into the forge
  UI to wire anything up.

- **Flake-native, no `hydraJobs` shim.** argunix evaluates
  `packages.<system>`, `checks.<system>`, `devShells.<system>`, and
  `nixosConfigurations.<name>` (built via
  `config.system.build.toplevel`) directly through `nix-eval-jobs`.
  Your flake's public surface _is_ the CI job set; there is no
  second list to maintain.

- **Safe by default for third-party PRs.** Building a stranger's
  Nix expression with build access is RCE. argunix gates every PR
  through a [live forge-permission check **plus** a static per-repo
  allowlist](docs/concepts/allowlist.md) — either passing grants
  the build, both failing rejects it.

- **No wasted work on a hot branch.** A new push to a PR branch
  [cancels the in-flight evaluation of the previous
  SHA](docs/concepts/cancel-on-push.md), and
  [duplicate webhook deliveries are coalesced](docs/concepts/webhook-coalescing.md)
  so a force-push-then-push burst doesn't queue two identical
  evaluations.

- **Sane forge UI for monorepos.** Posting one check per
  derivation is fine until your evaluation produces thousands of
  jobs. Past a threshold argunix
  [collapses to a single rolling check](docs/concepts/collapsed-checks.md)
  with a debounced markdown summary, while keeping per-job logs
  available behind a link.

- **Honest behavior when a token breaks.** A `401` from a forge
  [pauses dispatch for that forge](docs/concepts/forge-pause.md)
  instead of pummeling the operator's account with retries — the
  state is loud on the status page and clears as soon as the next
  permission lookup succeeds.

- **Per-eval, top-level-only DAG.** argunix knows when two
  top-level Jobs share a build dependency and
  [gates them so the dependency only builds once](docs/concepts/scheduling.md),
  while leaving internal-closure dedup to the substituter. The
  scheduler stays small; the database stays small; the operator
  still wins on the visible duplicate-work case.

- **Store retention without surprises.** Successful builds get
  a [GC root](docs/concepts/gc-roots.md) under
  `/nix/var/nix/gcroots/argunix/<repo>/<eval>/<job>`; failed and
  cancelled builds get none. Retention purges drop terminal
  evaluations in whole subtrees once configured age or size
  thresholds are exceeded.

<!-- TODO screenshot: a repository page with eval history and per-job badges, illustrating the "what changed in this push" view. -->

A deeper tour of these design choices lives in
[docs/concepts/](docs/concepts/). The
[configuration guide](docs/configuration.md) covers the YAML/NixOS
module options and which forge token scopes are actually used, and the
[container-images guide](docs/images.md) shows flake authors how to
build, multi-arch, and SBOM their images.

## Quickstart

This walks through deploying argunix on a NixOS host with one GitHub
repo. The same shape works for GitLab and Forgejo — only the forge
block changes.

### 1. Add the flake input

```nix
# flake.nix on your host configuration
{
  inputs.argunix.url = "git+https://codeberg.org/tfc/argunix";

  outputs = { self, nixpkgs, argunix, ... }: {
    nixosConfigurations.my-ci = nixpkgs.lib.nixosSystem {
      modules = [
        argunix.nixosModules.default
        { nixpkgs.overlays = [ argunix.overlays.default ]; }
        ./configuration.nix
      ];
    };
  };
}
```

### 2. Drop a forge token where systemd can read it

Generate a token with the scopes documented in
[docs/configuration.md](docs/configuration.md) (for GitHub a fine-grained
PAT with `Contents: read`, `Commit statuses: read/write`,
`Webhooks: read/write`, `Pull requests: read` is enough) and place it
at a path the `argunix` user can read:

```sh
install -m 0400 -o argunix -g argunix /tmp/gh-token \
  /var/lib/argunix-credentials/gh-token
```

### 3. Enable the service

```nix
{
  services.argunix = {
    enable = true;
    listen = "127.0.0.1:8080";
    settings = {
      external_url = "https://ci.example.com";
      forges.github = {
        kind = "github";
        web_url = "https://github.com";
        token_path = "/var/lib/argunix-credentials/gh-token";
        repos = {
          "you/your-flake" = { }; # default: build main + all PRs
          # "you/your-flake".watched_branches = [ "main" "release/*" ];
        };
      };
    };
  };

  # Front argunix with a reverse proxy that terminates TLS.
  services.nginx.virtualHosts."ci.example.com" = {
    enableACME = true;
    forceSSL = true;
    locations."/".proxyPass = "http://127.0.0.1:8080";
    locations."/".extraConfig = ''
      client_max_body_size 32m;   # GitHub webhooks can be up to 25 MB
      proxy_read_timeout 120s;
    '';
  };
}
```

`nixos-rebuild switch`, and that's it: argunix auto-installs the
webhook on each repo in `repos`, the first push triggers an
evaluation, and per-job statuses start appearing on the forge.

### 4. (Optional) Add a remote builder

By default argunix builds locally as a `trusted-users` member of the
host's Nix daemon. Each extra builder is an `argunix-builder` agent
running on a separate host: the agent dials **out** to argunix over
SSH, so the builder host has no inbound port to open, and argunix
treats the agent as a `nix-store --serve --write` backend per build
channel.

**1. On the argunix host**, expose the builder-enrollment SSH
listener and the shared enrollment token. Pick a long random
string, drop it at a path the `argunix` user can read, open the
port in the firewall, and reference both from the module:

```sh
head -c 32 /dev/urandom | base64 \
  | install -m 0400 -o argunix -g argunix /dev/stdin \
    /var/lib/argunix-credentials/builder-enrollment-token
```

```nix
{
  services.argunix.settings.builder_enrollment = {
    listen = "[::]:45678";
    token_path = "/var/lib/argunix-credentials/builder-enrollment-token";
  };
  networking.firewall.allowedTCPPorts = [ 45678 ];
}
```

**2. On the builder host**, place the _same_ token at a path the
`argunix-builder` user can read, then enable the builder module:

```nix
{
  imports = [ argunix.nixosModules.argunix-builder ];

  services.argunix-builder = {
    enable = true;
    argunixHost = "ci.example.com";
    argunixPort = 45678;
    enrollmentTokenFile = "/var/lib/argunix-builder/enrollment-token";
    # name = "ryzen-01";  # defaults to the machine's hostname
  };
}
```

On first connect the agent presents the enrollment token and
registers its persistent ed25519 pubkey; from then on it
authenticates by key, and the token file can be wiped (leaving it
in place is fine — the agent treats a missing file as
"pubkey-only"). The new builder appears on the status page and
starts receiving dispatch immediately.

To retire a builder, run `argunixctl builders revoke <name>` on
the argunix host; the agent's pubkey is invalidated and it falls
back to needing a fresh enrollment token to re-join.

### 5. (Optional) Publish builds to a binary cache

argunix can sign and push every successful build's output closure
to one or more binary caches, so the team substitutes from them
instead of rebuilding locally. The push fires on the coordinator
right after the output is pulled back from the builder, so a
multi-builder deployment only needs the signing key + storage
credentials in one place — see
[docs/concepts/cache-push.md](docs/concepts/cache-push.md).

**1. Generate a signing key** and drop the secret half where the
`argunix` user can read it:

```sh
nix --extra-experimental-features 'nix-command' \
  key generate-secret --key-name ci.example.com \
  | install -m 0400 -o argunix -g argunix /dev/stdin \
    /var/lib/argunix-credentials/cache/secret
nix --extra-experimental-features 'nix-command' \
  key convert-secret-to-public \
  < /var/lib/argunix-credentials/cache/secret \
  > /var/lib/argunix-credentials/cache/public
```

The contents of `…/cache/public` is what users put in their
`nix.settings.trusted-public-keys`.

**2. Point argunix at the cache.** For an S3-compatible backend
(real S3, Garage, MinIO, …), the push URL is the write endpoint;
`public_url` is the URL users will read from (typically a CDN or
the public gateway). `public_key` is what you generated in step 1
— argunix never reads the secret half at request time, so this
must be set explicitly. Symmetric backends (cachix, attic, plain
`file://`) leave `public_url` unset.

```nix
{
  services.argunix.settings.binary_caches = [
    {
      push_url = "s3://my-cache?endpoint=https://s3.example.com&region=eu-central-1";
      public_url = "https://cache.example.com";
      public_key = "ci.example.com:<contents-of-cache/public>";
      signing_key_path = "/var/lib/argunix-credentials/cache/secret";
    }
  ];

  # AWS-style credentials reach `nix copy --to s3://…` via the
  # daemon's environment. The file is a standard credentials INI
  # block (`[default]\naws_access_key_id=…\naws_secret_access_key=…`).
  systemd.services.argunix.serviceConfig.EnvironmentFile =
    "/var/lib/argunix-credentials/cache/s3-credentials";
}
```

After `nixos-rebuild switch`, the next successful build pushes
its closure to the cache. Push failures are logged and the job
stays `Success` — argunix never fails a build because a cache
hiccupped.

**3. Hand users the substituter snippet.** Once `public_url` and
`public_key` are set, the argunix instance renders ready-to-paste
snippets at `https://<your-deploy>/cache` — one for adding the
cache to a flake's `nixConfig`, one for a NixOS module, and one
for a plain `nix.conf` on macOS / generic Linux / WSL. Send users
the URL or the snippet they need.

## For Hydra and Botanix users

argunix occupies the same problem space as
[Hydra](https://github.com/NixOS/hydra) and
[Botanix](https://git.afnix.fr/botanix/botanix) — build Nix
derivations triggered by a forge — but takes different positions on
a handful of axes. If you've operated one of those, this is the
fastest way to know what to expect:

### Versus Hydra

**Similar:**

- Evaluates Nix expressions per change and posts per-job statuses.
- Distributes builds to remote machines.
- Maintains GC roots for successful builds and lets `nix-collect-garbage`
  reclaim everything else.
- Open source, GPL-licensed.

**Different:**

|                          | Hydra                                                    | argunix                                                                                                                              |
| ------------------------ | -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| State store              | PostgreSQL                                               | SQLite                                                                                                                               |
| Implementation           | Perl + Rust queue runner                                 | Rust workspace (single binary family)                                                                                                |
| Project model            | Operator-managed projects/jobsets, poll-based git inputs | Forge-webhook-driven; one entry per repo in YAML                                                                                     |
| Eval target              | `hydraJobs` attribute                                    | flake `packages` / `checks` / `devShells` / `nixosConfigurations`                                                                    |
| Dispatch granularity     | Per-`.drv` (full closure exploded into Steps)            | Per top-level Job, with [top-level → top-level DAG gating](docs/concepts/scheduling.md)                                              |
| Internal-closure dedup   | Yes, global via the Step map                             | Deferred to substituter / post-build cache                                                                                           |
| Forge integration        | Plugin to post statuses; PR support via separate setup   | GitHub / GitLab / Forgejo first-class; auto-installs webhooks; [PR allowlist + permission gate](docs/concepts/allowlist.md) built in |
| Configuration            | DB-backed; web admin UI to create projects/jobsets       | Declarative YAML / NixOS module; no admin UI for project setup                                                                       |
| Cancel-on-new-push       | No                                                       | [Yes](docs/concepts/cancel-on-push.md)                                                                                               |
| Many-job UX on the forge | One check per build                                      | [Collapsed rolling check past a threshold](docs/concepts/collapsed-checks.md)                                                        |

The headline trade is **dispatch granularity**. Hydra explodes every
top-level build into per-`.drv` Steps and dedups them globally; this
is exactly what you want for nixpkgs-shaped workloads with tens of
thousands of overlapping derivations. argunix dispatches at top-level
Job granularity and lets your binary cache do internal dedup — the
scheduler stays small, the database stays small, and the operator
pays for one extra rebuild of an internal drv on each fresh builder
that hasn't seen it yet. For small-to-medium project sets that's a
good trade; for "we are nixpkgs," Hydra is still the right tool.

### Versus Botanix

**Similar:**

- Rust implementation, webhook-driven, designed around Git forges.
- Per-eval DAG over discovered top-level jobs.
- Distributed builders, with the coordinator handing work out.

**Different:**

|                       | Botanix                                            | argunix                                                                                                                 |
| --------------------- | -------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| Builder transport     | gRPC (tonic) coordinator ↔ worker                 | Closure transfer over SSH (russh) from coordinator to enrolled builders + local trusted-user dispatch                   |
| Builder enrollment    | Worker registers via HTTP, receives a token        | Builder enrolls via a token + listen socket on the builder host; see [`nix/builder-module.nix`](nix/builder-module.nix) |
| Configuration surface | Environment variables                              | YAML / NixOS module options                                                                                             |
| Eval target           | `hydraJobs`                                        | flake `packages` / `checks` / `devShells` / `nixosConfigurations`                                                       |
| Forges out of the box | Forgejo (primary), GitHub / Gitea / Gerrit modules | GitHub + GHES, GitLab.com + self-hosted, Forgejo / Gitea / Codeberg                                                     |
| State store           | SQLite (via sea-orm)                               | SQLite (via sqlx)                                                                                                       |
| PR trust model        | n/a                                                | [Forge-permission check + static allowlist](docs/concepts/allowlist.md)                                                 |
| Auth failure handling | n/a                                                | [Forge pause on `401`](docs/concepts/forge-pause.md)                                                                    |
| Webhook duplicates    | n/a                                                | [Coalesced by `(repo, sha)`](docs/concepts/webhook-coalescing.md)                                                       |
| Many-job UX           | One check per build                                | [Collapsed rolling check past a threshold](docs/concepts/collapsed-checks.md)                                           |
| License               | EUPL-1.2                                           | GPL-3.0-or-later                                                                                                        |

The headline difference is **operational shape**. Botanix is a
coordinator-with-workers system you configure via env vars and stand
up with `cargo run`; argunix is a NixOS-module-first deployment with
forge-integration features (allowlists, cancel-on-push, forge-pause,
collapsed checks) wired into the daemon rather than bolted on.

## Maintained by

argunix is built and maintained by **tfc** at
**[Applicative Systems Group](https://applicative.systems)**.
Issues, patches, and `argunix` deployment war stories are welcome.

## License

argunix is licensed under
[GPL-3.0-or-later](https://www.gnu.org/licenses/gpl-3.0.html).
