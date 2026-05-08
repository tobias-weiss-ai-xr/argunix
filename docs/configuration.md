# argunix configuration

## Forges

Each forge entry under `forges:` in `argunix.yaml` needs an
authentication credential. Today only personal access tokens (PATs)
are supported in production; GitHub-App auth is reserved for a later
milestone.

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
_paused_ state for that forge (per Q82) and stops dispatching
work — surfaced in the daemon log and on the status page. Rotate
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
