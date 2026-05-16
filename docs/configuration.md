# argunix configuration

## Forges

Each forge entry under `forges:` in `argunix.yaml` needs an authentication credential.
Today only personal access tokens (PATs) are supported in production.

### What argunix actually does with the token

Knowing the calls argunix makes is the easiest way to reason about
which scopes you need:

1. **Auto-install a webhook** on each configured repo. argunix
   generates the secret, registers the hook against
   `<your-deploy>/webhook/<forge_kind>`, and re-PATCHes if anything
   drifts. → needs **hook-management** permission.
2. **Post commit statuses / checks** as builds progress (`pending`,
   `success`, `failure`). → needs **commit-status write**
   permission.
3. **Read collaborator / member permission** before evaluating
   third-party PRs (the untrusted-PR check). → needs **collaborator
   read** permission.
4. **Read PR metadata** (number, head SHA, head ref) on
   pull-request webhooks where the payload alone is insufficient.
   → needs **PR read** permission.
5. **Clone over HTTPS** using the same token embedded in the URL.
   → needs **repository contents read**.

If a configured forge starts returning 401, argunix flips into the
_paused_ state for that forge (see [concepts/forge-pause.md](concepts/forge-pause.md))
and stops dispatching work — surfaced in the daemon log and on the
status page. Rotate
the token, drop the new value into the path referenced by
`token_path`, and `argunixctl reload`.

### Where to put the token

Use `LoadCredential=` in the NixOS module so systemd materialises
the file under `$CREDENTIALS_DIRECTORY` at unit start; the token
never lives in the world-readable nix store.

```nix
services.argunix.credentials.gh-token = "/etc/argunix/secrets/gh-token";
services.argunix.settings.forges.github = {
  kind = "github";
  web_url = "https://github.com";
  token_path = "$CREDENTIALS_DIRECTORY/gh-token";
};
```

The file should contain the token and nothing else. A trailing
newline is tolerated; everything else (whitespace, comments) is
not.

---

### GitHub

Generate at <https://github.com/settings/tokens> (classic) or under
_Settings → Developer settings → Personal access tokens →
Fine-grained tokens_.

**Classic PAT** — simplest:

| Scope  | Why argunix needs it                                                                       |
| ------ | ------------------------------------------------------------------------------------------ |
| `repo` | Covers private-repo cloning, commit statuses, hook management, and PR metadata in one box. |

That's it. `repo` is a coarse scope; pick fine-grained below if you
care.

**Fine-grained PAT** — recommended for production:

Pick one token per organisation or set of repositories. Under
_Repository permissions_, set:

| Permission      | Access         | What it covers                         |
| --------------- | -------------- | -------------------------------------- |
| Metadata        | Read           | implied by every other scope; required |
| Contents        | Read           | HTTPS clone of the source tree         |
| Commit statuses | Read and write | per-job `argunix: <attr>` checks       |
| Webhooks        | Read and write | webhook auto-install + drift repair    |
| Pull requests   | Read           | PR head SHA / head ref lookups         |

For PR author trust checks on **public** repos with PRs from
forks, no extra scope is needed (the public collaborator endpoint
is unauthenticated). For PR checks on **private** repos:

| Permission | Access |                                              |
| ---------- | ------ | -------------------------------------------- |
| Members    | Read   | needed by the collaborator-permission lookup |

GitHub Enterprise Server: the same scopes apply. Set `web_url` to
your GHES hostname (e.g. `https://ghe.example.com`); argunix
derives the API URL as `<web_url>/api/v3`.

---

### GitLab

Generate at _User Settings → Access Tokens_ (or for a single
project: _Project Settings → Access Tokens_; the project scope is
narrower and preferred).

**Required scope:**

| Scope | Why argunix needs it                                                                                                                                                                                                                                   |
| ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `api` | Single scope that covers commit-status posting (`/projects/:id/statuses/:sha`), webhook management (`/projects/:id/hooks`), MR metadata, and member-permission lookups. GitLab does not break these out into finer scopes for the calls argunix makes. |

`read_api` is **not** sufficient — posting commit statuses and
managing webhooks both require write access via `api`.

**Project role**: the user the PAT belongs to must be **Maintainer
or Owner** on the project — that's a GitLab requirement for
creating webhooks, independent of token scope. If `api` scope is
present but webhook installation 403s, role is the cause.

For self-hosted GitLab: set `web_url` to the instance hostname
(e.g. `https://gitlab.example.com`); argunix derives the API URL
as `<web_url>/api/v4`.

**Reusing the same token for registry pushes**: the `api` scope
already grants read/write access to the GitLab container registry,
so the _same_ token that drives the forge can also authenticate the
`registry-push` effect (`registries.<name>.auth_path`) — no second
token, no extra scope. The standalone `read_registry` /
`write_registry` scopes only matter for tokens that do _nothing
else_; with `api` present they are redundant.

The two consumers want the token in different file formats, so write
it to two files:

| Consumer        | Config key                 | File contents        |
| --------------- | -------------------------- | -------------------- |
| Forge API       | `forges.<f>.token_path`    | the token, bare      |
| `registry-push` | `registries.<r>.auth_path` | `<username>:<token>` |

For the `auth_path` file the username depends on the token type: a
**project or group access token** uses the token's _name_ as the
username; a **personal access token** uses your GitLab username.
`registry-push` reads `auth_path` at push time, hands it to
`skopeo --dest-creds`, and never logs it.

```sh
printf '%s' "$TOKEN"          > /var/lib/argunix-credentials/opencode-token
printf '%s' "argunix:$TOKEN"  > /var/lib/argunix-credentials/opencode-registry-creds
chmod 600 /var/lib/argunix-credentials/opencode-*
```

---

### Forgejo / Gitea / Codeberg

Generate at _Settings → Applications → Manage Access Tokens_.

**Required scopes:**

| Scope              | Why argunix needs it                                |
| ------------------ | --------------------------------------------------- |
| `read:repository`  | PR / collaborator-permission lookups, source clone. |
| `write:repository` | Webhook management and commit-status posting.       |

`read:user` is **not** required for any current call.

**Repository admin access**: like GitLab, hook creation requires
admin permission on each repository, not just the right scope on
the token. The token's user must own the repo or be added with
admin role.

**Known Forgejo limitation**: PATCHing an existing webhook via
`/repos/:owner/:repo/hooks/:id` does **not** update its events
list (config events show as updated but the hook keeps firing on
the original set). Workaround: if you change `watched_branches` /
`build_prs` config and need the forge-side hook to follow, delete
the hook in the Forgejo UI and let argunix's auto-install
re-create it on the next reload.

Set `web_url` to the instance hostname (e.g. `https://codeberg.org`,
`https://forgejo.example.com`); argunix derives the API URL as
`<web_url>/api/v1`.

## Registries

`registries:` is a named catalog of external docker registries the
`registry-push` effect copies built container images to. A repo opts
in via `push_to_registries` (settable per repo, per forge, or in
`defaults` — the lists merge). Each entry:

| Field       | Meaning                                                                 |
| ----------- | ----------------------------------------------------------------------- |
| `url`       | Registry host, no scheme — `ghcr.io`, `registry.example.com:5000`.      |
| `namespace` | Path segment images land under (see `{slug}` below).                    |
| `auth_path` | File with one `user:password` line for `skopeo --dest-creds`. Optional. |
| `insecure`  | Skip TLS verification — for a plain-HTTP registry. Defaults to `false`. |

An image is pushed to `<url>/<namespace>/<image>:<tag>`, where
`<image>` is the build attribute's leaf name and `<tag>` is the branch
name plus an immutable `sha-<short>` tag.

### Marking a build as an image

argunix only treats a build output as a container image — and only
then runs `registry-push` — when its derivation declares the
`meta.image-format` attribute:

```nix
# a docker save / dockerTools.buildLayeredImage tarball
meta.image-format = "docker";

# an OCI image-layout archive, possibly a multi-arch index
meta.image-format = "oci";
```

The value selects the `skopeo` transport: a `docker` build is pushed
from a `docker-archive:`, an `oci` build from an `oci-archive:` with
`--multi-arch all` so a manifest list is copied whole. A build without
`meta.image-format` is an ordinary package and is never pushed. argunix
does not sniff the archive — the format is declared, not guessed.

Note: argunix's own embedded read-only registry ingests `docker`
images only; `oci` images are distributed solely through the
`registry-push` effect.

### The `{slug}` namespace placeholder

`namespace` may contain a `{slug}` placeholder. The effect substitutes
the building repo's slug at push time, so **one catalog entry can
serve many repos**:

```yaml
registries:
  opencode:
    url: registry.opencode.de
    namespace: "{slug}"
    auth_path: /var/lib/argunix-credentials/opencode-registry-creds
```

This matters because the right namespace differs by registry:

- **GitLab** (gitlab.com, self-hosted, opencode.de): the registry path
  _is_ the project path, and an argunix repo's slug already equals its
  project path — so `namespace: "{slug}"` pushes each repo under its
  own project, which is also the only path the per-project push
  permission allows.
- **ghcr.io**: `{slug}` yields the conventional
  `ghcr.io/<owner>/<repo>/<image>` layout. A literal `namespace: myorg`
  also works, but then every bound repo shares it — two repos with the
  same image attribute name would collide.
- **Docker Hub / generic registries**: a literal namespace is usually
  what you want; reach for `{slug}` only if you need per-repo paths.

A namespace with no `{slug}` is used verbatim for every bound repo.
