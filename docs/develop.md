# developing the web UI

The `argunix-web-dev` binary boots the read-only HTTP UI against an
in-memory SQLite seeded with fixture data — no daemon, no real
builders, no forge integration. You get the same axum router, the same
askama templates, and the same Tailwind stylesheet that ship in
production, so the layout you see while iterating matches what an
operator sees.

## one-time setup

The dev binary needs `tailwindcss` and `cargo-watch` on `PATH`. Neither
is in the project's `devShells.default` today, so layer them on:

    nix shell nixpkgs#tailwindcss_4 nixpkgs#cargo-watch

(Inside that shell every command below works as written.)

## launching

The dev binary expects to be run from `argunix-web/` — it resolves
`static/input.css`, `static/ui.css`, and `templates/` relative to the
current directory and bails with a usage hint if those aren't in
sight.

    cd argunix-web
    cargo run --bin argunix-web-dev

You'll see roughly:

    tailwindcss --watch: pid 12345
    argunix dev UI listening on http://127.0.0.1:8080/
        /            – cluster status (htmx-polled)
        /repos       – repos overview
        /r/gh/argunix/argunix
        ...

Open the printed URL. To pick a different port:

    ARGUNIX_DEV_LISTEN=127.0.0.1:9000 cargo run --bin argunix-web-dev

## what you get

The binary writes a fresh in-memory SQLite at boot and seeds enough
fixture rows to hit every interesting branch in every template:

- **/repos** — four repos: one without a description (so the "—"
  fallback renders), one with no eval at all (so the "Awaiting first
  evaluation" table renders), and two with a full eval history.
- **/** (status) — five builders covering every state: two `online`,
  one `draining`, one `offline`, one `revoked`. Three running jobs
  pinned to the `push` / `build` / `pull` phase badges, plus queued
  rows.
- **eval pages** — a Done eval with a mix of job outcomes, an
  in-progress Building eval triggered by PR #42, a Cancelled eval, and
  an EvaluationFailed eval with a multi-line `failure_reason` callout.
- **job pages** — a Success job with `output_path` and phase metrics,
  a Failure job, a live-running job (so the "live builder" SSE
  placeholder renders).

Edit `argunix-web/src/bin/dev.rs` to bend any of this.

## hot-reload

Two concerns, two tools, no feedback loops:

### CSS — handled automatically

The dev binary spawns `tailwindcss -i static/input.css -o static/ui.css
--watch=always` as a child process and reaps it (via
`PR_SET_PDEATHSIG`) when it exits. Edit a template's class list or
`input.css`; tailwind notices the change, rewrites `static/ui.css`,
and a browser refresh shows the new styles. No server restart needed.

If `tailwindcss` isn't on `PATH` the dev binary prints a warning and
serves the existing `static/ui.css` as-is — you can still iterate on
HTML structure without it.

### HTML / Rust — `cargo watch`

Askama bakes templates into the binary at compile time, so a template
edit needs a rebuild. Run this in a second terminal:

    cd argunix-web
    cargo watch -w src -w templates -x 'run --bin argunix-web-dev'

That watches `argunix-web/src/**` (the dev binary, the route handlers,
the askama-derive structs) and `argunix-web/templates/**` (the HTML
itself). Anything you change there triggers a recompile + relaunch.

Crucially, **not** watching `argunix-web/static/**`: that's where
tailwind writes `ui.css`, and feeding its output back into cargo-watch
would loop forever.

## browser refresh

There's no live-reload script injected into the page yet — refresh
manually after a CSS or template edit. Adding `tower-livereload`
would be a small change if it gets annoying.

## what the dev binary does _not_ exercise

- **Webhooks** (`POST /webhook/<forge_kind>`) — handlers are mounted
  but the worker dispatcher channel is a black hole. Hits won't crash
  but they won't trigger any work either.
- **Live SSE log tailing** — the `LiveLogRegistry` is empty; the SSE
  endpoint will 404 since no build is actually streaming chunks.
- **Forge API calls** — the providers map is empty. Anything that
  would normally hit GitHub / GitLab / Forgejo just isn't reachable.

For all three, run the real daemon instead.
