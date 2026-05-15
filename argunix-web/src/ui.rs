//! Read-only HTML UI.
//!
//! Routes:
//!   GET /                                                       — index, list of repos
//!   GET /r/<forge>/<...slug>                                    — repo page (recent evals)
//!   GET /r/<forge>/<...slug>/eval/<id>                          — eval detail (job table)
//!   GET /r/<forge>/<...slug>/eval/<id>/job/<attr>               — single job detail
//!   GET /r/<forge>/<...slug>/eval/<id>/job/<attr>/log           — decompressed build log
//!
//! All `/r/...` paths share a single axum catch-all (`/r/{forge}/{*tail}`)
//! and dispatch on segment markers (`/eval/`, `/job/`) — that's the
//! only way to support gitlab-subgroup slugs that contain slashes
//! without enumerating every depth ahead of time.
//!
//! Markup lives in `argunix-web/templates/*.html` and is rendered with
//! Askama (compile-time, type-checked). Static assets — including the
//! Tailwind-compiled `ui.css` referenced by `base.html` — are served
//! separately by a `ServeDir` mounted at `/static` (see `lib.rs`).

use crate::state::AppState;
use argunix_builders::ConnState;
use argunix_config::ForgeConfig;
use argunix_domain::{EvalId, EvalStatus, JobStatus, Slug};
use argunix_store::{BuilderStore, EvalStore, JobStore, RepoStore};
use askama::Template;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use std::collections::HashMap;

#[derive(Template)]
#[template(path = "status.html")]
struct StatusTemplate {
    /// Drives the header logo's `argunix-spin` class in `base.html`.
    /// True iff anything is `Evaluating` or `Running` at render time.
    cluster_active: bool,
    totals: ClusterTotals,
    builders: Vec<BuilderRow>,
    evaluating: Vec<EvalRow2>,
    eval_queue_depth: usize,
    /// Up to `UPCOMING_EVALS_LIMIT` queued evals, oldest first — what
    /// the worker will pick up next. Length may be < `eval_queue_depth`
    /// when truncated; the template uses both to render
    /// "showing N of M" copy.
    upcoming_evals: Vec<EvalRow2>,
    running: Vec<RunningRow>,
    queued: Vec<QueuedRow>,
    queued_shown: usize,
    queued_truncated: bool,
}

/// Dedicated `/hosts` page. Renders one card for the argunix
/// coordinator host (cpu / load / mem sparklines polled from
/// `/api/host/stats`) followed by one card per builder. Builders
/// also carry capability badges and the full current-jobs list.
#[derive(Template)]
#[template(path = "hosts.html")]
struct HostsTemplate {
    /// Drives the header logo's `argunix-spin` class. True iff any
    /// builder has at least one current job in flight.
    cluster_active: bool,
    /// The argunix coordinator card. Always rendered, even when the
    /// host has no live samples yet (template falls back to "—").
    coordinator: CoordinatorRow,
    rows: Vec<BuilderRow>,
    online: usize,
    known: usize,
}

/// Coordinator-host row for the `/hosts` page header card. Capability
/// fields are absent because the coordinator doesn't run builds
/// itself — it only orchestrates them. The two version fields are
/// the coordinator's own `nix` / `nix-eval-jobs` toolchain (used to
/// drive evaluations and store ops); builders report their own
/// `nix_version` separately on each card below.
struct CoordinatorRow {
    /// Display name shown on the card. Pulled from `gethostname()` at
    /// render time; falls back to `"argunix"` if the syscall fails.
    hostname: String,
    /// Wall-clock since daemon startup, humanized
    /// (`"3h 12m"` / `"42s"`).
    uptime: String,
    /// `nix --version` token resolved at daemon startup. "unknown"
    /// if detection failed or the dev fixture didn't populate it.
    nix_version: String,
    /// `nix-eval-jobs --version` token resolved at daemon startup.
    /// "unknown" under the same conditions.
    nix_eval_jobs_version: String,
}

/// Polled fragment of the status page: the section content plus an
/// `hx-swap-oob` image that re-targets the header logo, so the
/// `argunix-spin` class tracks `cluster_active` between full
/// navigations. The wrapper template (`_status_fragment.html`)
/// emits the OOB image and then `{% include %}`s the same partial
/// (`_status_inner.html`) that the full page renders inline — so
/// the section markup stays defined in exactly one place.
#[derive(Template)]
#[template(path = "_status_fragment.html")]
struct StatusInnerTemplate {
    /// Drives the OOB image's `argunix-spin` class. Recomputed each
    /// poll from the same predicate the full-page render uses.
    cluster_active: bool,
    totals: ClusterTotals,
    builders: Vec<BuilderRow>,
    evaluating: Vec<EvalRow2>,
    eval_queue_depth: usize,
    upcoming_evals: Vec<EvalRow2>,
    running: Vec<RunningRow>,
    queued: Vec<QueuedRow>,
    queued_shown: usize,
    queued_truncated: bool,
}

/// Status-page row for an evaluation in `Evaluating` (or, when
/// `eval_queue_depth > 0`, the head-of-queue we surface alongside).
/// Distinct from the `EvalRow` used by the repo page so the column
/// layouts can diverge — the status-page version shows trigger and
/// elapsed-since-started, repo page shows finished_at.
struct EvalRow2 {
    eval_id: i64,
    forge: String,
    slug: String,
    git_ref: String,
    short_sha: String,
    trigger: String,
    started: String,
}

struct ClusterTotals {
    builders_online: usize,
    builders_known: usize,
    running: usize,
    queued_total: usize,
}

struct BuilderRow {
    name: String,
    status: &'static str,
    status_class: &'static str,
    is_online: bool,
    in_flight: u32,
    max_jobs: u32,
    /// Rendered as small pill badges in templates. Stored as
    /// `Vec<String>` rather than a comma-joined string so the template
    /// can iterate.
    systems: Vec<String>,
    features: Vec<String>,
    nix_version: String,
    /// Suppressed in the template when `is_online` is true (we already
    /// know it's live; "last seen" reads as past tense).
    last_seen: String,
    /// Builds currently dispatched to this builder. Populated for
    /// online builders only; offline / revoked rows always carry an
    /// empty list. Cards render the head as a "now building" line and
    /// a count for the rest.
    current_jobs: Vec<CurrentJob>,
}

/// One in-flight job on a builder, as carried by [`BuilderRow`].
struct CurrentJob {
    attr_path: String,
    eval_id: i64,
    forge: String,
    slug: String,
    phase: &'static str,
    phase_class: &'static str,
}

struct RunningRow {
    forge: String,
    slug: String,
    eval_id: i64,
    attr_path: String,
    system: String,
    git_ref: String,
    short_sha: String,
    builder: String,
    started: String,
    /// Live transport/build phase for jobs currently dispatched to a
    /// pool builder. `"push"`, `"build"`, `"pull"`, or empty for
    /// jobs that are running locally / haven't reached a pool builder
    /// yet. Sourced from `BuilderRegistry::phase_snapshot()`.
    phase: &'static str,
    phase_class: &'static str,
}

struct QueuedRow {
    forge: String,
    slug: String,
    eval_id: i64,
    attr_path: String,
    system: String,
    git_ref: String,
    short_sha: String,
}

/// Hard cap on the upcoming-jobs table — keeps the page bounded under
/// large queues. Anything beyond this just gets summarised in the count.
const QUEUED_DISPLAY_LIMIT: u32 = 50;

/// Hard cap on the upcoming-evaluations table on the status page.
/// Eval queue depth is normally small (single-digit), so this is more
/// of a safety net than a typical case.
const UPCOMING_EVALS_LIMIT: u32 = 20;

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    cluster_active: bool,
    /// Configured repos that have produced at least one evaluation —
    /// the table renders the full set of columns (description, latest
    /// eval link, etc.).
    active_repos: Vec<RepoRow>,
    /// Configured repos that have not yet produced any evaluation.
    /// Either no webhook has arrived, or every webhook so far was
    /// dropped by policy (unwatched branch, untrusted PR author, …).
    /// Surfaced separately so operators can spot misconfigured
    /// webhooks at a glance.
    pending_repos: Vec<RepoRow>,
}

struct RepoRow {
    forge: String,
    /// Link target for the forge column — points at the repo's
    /// owning org / namespace on the forge web UI. Empty string when
    /// no forge config matches (template suppresses the link).
    forge_url: String,
    slug: String,
    /// Project page on the forge. Empty when we have no `web_url`
    /// for this repo and no forge config to fall back to; template
    /// degrades the slug to plain text in that case.
    repo_url: String,
    description: Option<String>,
    /// `Some(id)` if the repo has at least one evaluation; the template
    /// renders a "latest eval" link to it.
    latest_eval_id: Option<i64>,
}

#[derive(Template)]
#[template(path = "repo.html")]
struct RepoTemplate {
    cluster_active: bool,
    forge: String,
    slug: String,
    name: Option<String>,
    description: Option<String>,
    /// Project page on the forge. Empty when neither
    /// `repo.web_url` nor a forge config is available; template
    /// suppresses the link in that case.
    repo_url: String,
    /// Absolute URL of the SVG status badge for this repo, default
    /// branch (i.e. any branch — `pick_eval` picks the latest
    /// terminal eval). Surfaced on the repo page so users can copy a
    /// markdown snippet into their README.
    badge_url: String,
    /// Markdown snippet for `[![argunix](badge_url)](repo_url)`. Copy
    /// button on the page writes this to the clipboard.
    badge_markdown: String,
    evals: Vec<EvalRow>,
}

struct EvalRow {
    id: i64,
    git_ref: String,
    short_sha: String,
    status: &'static str,
    finished: String,
    /// Wall-clock between `started_at` and `finished_at`, humanized.
    /// `"—"` when either is missing.
    total: String,
    /// PR number if this eval was triggered by a pull/merge request,
    /// otherwise `None`. Drives the `git_ref` cell rendering — PRs
    /// show as "PR #N" rather than the synthetic ref shape.
    pr_number: Option<u32>,
    /// Forge-side link target for this eval row. When `pr_number` is
    /// set, points at the PR; otherwise at the branch. Empty when
    /// no forge URL can be constructed (no webhook + no forge cfg).
    forge_link: String,
    /// Forge-side link target for the eval's commit SHA. Empty when
    /// no forge URL can be constructed.
    commit_link: String,
}

#[derive(Template)]
#[template(path = "eval.html")]
struct EvalTemplate {
    cluster_active: bool,
    forge: String,
    slug: String,
    eval_id: i64,
    status_label: &'static str,
    phase_class: &'static str,
    trigger: String,
    git_ref: String,
    sha: String,
    started: String,
    finished: String,
    /// Wall-clock between `started_at` and `finished_at`, humanized.
    total: String,
    /// Wall-clock between `started_at` and `building_started_at` —
    /// the eval-only portion of the run. `"—"` when the row hasn't
    /// reached `Building` (still evaluating, or eval-failed /
    /// cancelled mid-eval), or pre-dates the column.
    eval_time: String,
    /// Wall-clock between `building_started_at` and `finished_at` —
    /// the build-only portion. Same fallback rules as `eval_time`.
    build_time: String,
    job_heading: String,
    empty_jobs_msg: &'static str,
    jobs: Vec<JobRow>,
    /// PR number when this eval was triggered by a pull/merge request.
    pr_number: Option<u32>,
    /// Project page on the forge. Empty when no forge URL can be
    /// constructed; template degrades the project link to plain text.
    repo_url: String,
    /// Worker-captured failure detail for `EvaluationFailed` rows
    /// (clone error, nix-eval-jobs stderr, …). `None` for every
    /// other status; the template hides the box when absent.
    failure_reason: Option<String>,
    /// Forge-side link to the PR (when `pr_number.is_some()`) or the
    /// branch (otherwise). Empty when no forge URL can be built.
    ref_link: String,
    /// Forge-side link to the commit. Empty when no forge URL can be
    /// built.
    commit_link: String,
}

struct JobRow {
    attr_path: String,
    system: String,
    status: &'static str,
    /// Unicode glyph rendered in place of the status text (✓, ✗, …).
    /// The visible text label moves into `aria-label` on the wrapping
    /// span so screen readers still announce the status.
    glyph: &'static str,
    /// Tailwind text-colour class for the glyph wrapper. Keyed off the
    /// same ok/fail/info/warn/muted buckets used by the eval-status
    /// pill so the palette stays consistent across the UI.
    glyph_class: &'static str,
    finished: String,
    has_log: bool,
    /// Per-job wall-clock; `"—"` when missing.
    duration: String,
}

#[derive(Template)]
#[template(path = "job.html")]
struct JobTemplate {
    cluster_active: bool,
    forge: String,
    slug: String,
    eval_id: i64,
    job_id: i64,
    attr_path: String,
    system: String,
    status_label: &'static str,
    started: String,
    finished: String,
    /// Wall-clock between `started_at` and `finished_at`, humanized.
    total: String,
    drv_path: Option<String>,
    output_path: Option<String>,
    has_log: bool,
    /// `Some(builder_name)` while this job is dispatched on a pool
    /// builder — drives the live stats sparkline + log SSE on the
    /// page. `None` for finished jobs and locally-built ones.
    live_builder: Option<String>,
    /// Per-phase transport accounting. Each pair is rendered as
    /// "<value> (<raw>)" already-formatted; absent fields surface as
    /// "—". The whole block is suppressed in the template when no
    /// phase has data, so jobs built locally stay clean.
    phase_metrics: PhaseMetricsRow,
}

#[derive(Default)]
struct PhaseMetricsRow {
    has_any: bool,
    push_bytes: String,
    push_ms: String,
    build_ms: String,
    pull_bytes: String,
    pull_ms: String,
}

pub async fn index(State(state): State<AppState>) -> Result<Html<String>, UiError> {
    let cluster_active = cluster_is_active(&state).await?;
    let snap = state.current.load_full();

    // Index DB rows by (forge, slug) so we can look up by configured
    // identity in O(1). Repos that landed in the DB but aren't (or no
    // longer are) in the config are dropped from the page — actual
    // row pruning happens in the retention pass; the index only shows
    // what the operator currently has configured.
    let db_rows = state.store.list().await?;
    let mut by_key: HashMap<(String, String), argunix_store::RepoRecord> = HashMap::new();
    for r in db_rows {
        by_key.insert((r.forge.clone(), r.slug.as_str().to_string()), r);
    }

    let mut active_repos: Vec<RepoRow> = Vec::new();
    let mut pending_repos: Vec<RepoRow> = Vec::new();

    for repo in &snap.config.repos {
        let forge_cfg = snap.config.forges.get(&repo.forge);
        let slug_str = repo.slug.as_str().to_string();
        let db = by_key.get(&(repo.forge.clone(), slug_str.clone()));

        let latest_eval_id = if let Some(rec) = db {
            <argunix_store::SqlxStore as EvalStore>::list_by_repo(&state.store, rec.id, 1)
                .await?
                .into_iter()
                .next()
                .map(|e| e.id.get())
        } else {
            None
        };

        let forge_url = forge_url_for(forge_cfg, repo.slug.as_str());
        let repo_url = repo_url_for(
            db.and_then(|r| r.web_url.as_deref()),
            forge_cfg,
            repo.slug.as_str(),
        );
        let row = RepoRow {
            forge: repo.forge.clone(),
            forge_url,
            slug: slug_str,
            repo_url,
            description: db.and_then(|r| r.description.clone()),
            latest_eval_id,
        };
        if latest_eval_id.is_some() {
            active_repos.push(row);
        } else {
            pending_repos.push(row);
        }
    }

    Ok(Html(render(&IndexTemplate {
        cluster_active,
        active_repos,
        pending_repos,
    })?))
}

/// Forge-column link: project namespace / org page on the forge.
/// Falls back to the forge root if the slug has no namespace
/// component, or to an empty string if no forge config exists.
fn forge_url_for(forge_cfg: Option<&ForgeConfig>, slug: &str) -> String {
    let Some(cfg) = forge_cfg else {
        return String::new();
    };
    let base = cfg.web_url.trim_end_matches('/');
    match slug.split_once('/') {
        Some((org, _)) => format!("{base}/{org}"),
        None => base.to_string(),
    }
}

/// Project page URL for a repo. Prefers the forge-supplied
/// `repo.web_url` (populated from webhook payloads); falls back to
/// `{forge.web_url}/{slug}` when no webhook has landed yet. Returns
/// an empty string when neither source is available.
pub(crate) fn repo_url_for(
    repo_web_url: Option<&str>,
    forge_cfg: Option<&ForgeConfig>,
    slug: &str,
) -> String {
    if let Some(u) = repo_web_url.filter(|s| !s.is_empty()) {
        return u.trim_end_matches('/').to_string();
    }
    let Some(cfg) = forge_cfg else {
        return String::new();
    };
    let base = cfg.web_url.trim_end_matches('/');
    format!("{base}/{slug}")
}

/// Cluster status overview — at-a-glance view of every known builder
/// and what the cluster is doing right now. The page wraps the section
/// content in an htmx polling div that swaps in fresh markup from
/// [`status_fragment`] every 5s — no full-page reloads.
pub async fn status(State(state): State<AppState>) -> Result<Html<String>, UiError> {
    let view = collect_status_view(&state).await?;
    let cluster_active = !view.evaluating.is_empty() || !view.running.is_empty();
    Ok(Html(render(&StatusTemplate {
        cluster_active,
        totals: view.totals,
        builders: view.builders,
        evaluating: view.evaluating,
        eval_queue_depth: view.eval_queue_depth,
        upcoming_evals: view.upcoming_evals,
        running: view.running,
        queued: view.queued,
        queued_shown: view.queued_shown,
        queued_truncated: view.queued_truncated,
    })?))
}

/// `GET /hosts` — dedicated page with the argunix coordinator card on
/// top and one rich card per builder below. Both card types carry live
/// sparklines (cpu / load1 / mem) — coordinator polls `/api/host/stats`,
/// builders poll `/api/builders/{name}/stats`. Builder cards also list
/// their current jobs with phase badges. Designed mobile-first: 1 col
/// on phones, 2 on tablets, 3 on desktop.
pub async fn hosts(State(state): State<AppState>) -> Result<Html<String>, UiError> {
    let view = collect_hosts_only(&state).await?;
    let cluster_active = view.rows.iter().any(|b| !b.current_jobs.is_empty());
    Ok(Html(render(&HostsTemplate {
        cluster_active,
        coordinator: build_coordinator_row(&state),
        rows: view.rows,
        online: view.online,
        known: view.known,
    })?))
}

#[derive(Template)]
#[template(path = "caches.html")]
struct CachesTemplate {
    /// `base.html` reads this to drive the header logo's spin
    /// animation. The cache page has no notion of "cluster busy",
    /// so always false — the spinner stays quiet on this page even
    /// when builds are running. The status / hosts pages remain the
    /// place to look for live activity.
    cluster_active: bool,
    /// Caches users can actually consume — every entry has
    /// `public_url` + `public_key`, so each carries pre-rendered
    /// snippets the template drops verbatim into copy buttons.
    public_caches: Vec<PublicCacheRow>,
    /// How many entries argunix pushes to that *cannot* be
    /// advertised to users yet (missing `public_url` or
    /// `public_key`). Rendered as a single operator-facing
    /// reminder line at the bottom, with no per-cache detail —
    /// this page is for users, not for diagnosing the YAML.
    incomplete_count: usize,
}

/// User-facing view of one fully-configured cache. `push_url` is
/// deliberately absent — that's an operator-internal endpoint
/// (often carrying credentials or pointing at a private S3 host)
/// and has no business on a page consumers might forward links to.
struct PublicCacheRow {
    public_url: String,
    public_key: String,
    /// `nixConfig` block to paste at the top of a `flake.nix`.
    flake_snippet: String,
    /// `nix.settings` block for a NixOS module.
    nixos_snippet: String,
    /// Plain `nix.conf` lines for non-NixOS hosts
    /// (`~/.config/nix/nix.conf` or `/etc/nix/nix.conf`).
    nix_conf_snippet: String,
}

/// `GET /cache` — list configured binary caches with copy-pasteable
/// substituter snippets so end-users can opt into the cache from a
/// flake, a NixOS config, or a plain `nix.conf`. Entries that lack
/// the public-side fields needed to render those snippets are
/// hidden from the main list and summarised as a count at the
/// bottom for the operator's eyes only.
pub async fn caches(State(state): State<AppState>) -> Result<Html<String>, UiError> {
    let snap = state.current.load_full();
    let mut public_caches = Vec::new();
    let mut incomplete_count = 0usize;
    for c in &snap.config.binary_caches {
        match (c.public_url.as_deref(), c.public_key.as_deref()) {
            (Some(url), Some(key)) => {
                public_caches.push(PublicCacheRow {
                    public_url: url.to_string(),
                    public_key: key.to_string(),
                    flake_snippet: render_flake_snippet(url, key),
                    nixos_snippet: render_nixos_snippet(url, key),
                    nix_conf_snippet: render_nix_conf_snippet(url, key),
                });
            }
            _ => incomplete_count += 1,
        }
    }
    Ok(Html(render(&CachesTemplate {
        cluster_active: false,
        public_caches,
        incomplete_count,
    })?))
}

fn render_flake_snippet(public_url: &str, public_key: &str) -> String {
    format!(
        "{{\n  nixConfig = {{\n    extra-substituters = [ \"{public_url}\" ];\n    extra-trusted-public-keys = [ \"{public_key}\" ];\n  }};\n\n  # ... rest of your flake ...\n}}"
    )
}

fn render_nixos_snippet(public_url: &str, public_key: &str) -> String {
    format!(
        "{{\n  nix.settings = {{\n    extra-substituters = [ \"{public_url}\" ];\n    extra-trusted-public-keys = [ \"{public_key}\" ];\n  }};\n}}"
    )
}

fn render_nix_conf_snippet(public_url: &str, public_key: &str) -> String {
    format!("extra-substituters = {public_url}\nextra-trusted-public-keys = {public_key}")
}

fn build_coordinator_row(state: &AppState) -> CoordinatorRow {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "argunix".to_string());
    let uptime = humanize_uptime(state.started_at.elapsed());
    CoordinatorRow {
        hostname,
        uptime,
        nix_version: state.coordinator_versions.nix_version.clone(),
        nix_eval_jobs_version: state.coordinator_versions.nix_eval_jobs_version.clone(),
    }
}

fn humanize_uptime(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    }
}

/// Builder list independent of the eval/queue queries the status page
/// runs. Shared between the `/hosts` page and the `/api/hosts` JSON
/// endpoint so both render exactly the same view.
async fn collect_hosts_only(state: &AppState) -> Result<HostsView, UiError> {
    let roster = <argunix_store::SqlxStore as BuilderStore>::list_all(&state.store).await?;
    let live = state.builder_registry.list();
    let phases = state.builder_registry.phase_snapshot();
    let running_jobs = <argunix_store::SqlxStore as JobStore>::list_running(&state.store).await?;

    let id_to_name: HashMap<i64, String> = roster
        .iter()
        .map(|r| (r.id.get(), r.name.as_str().to_string()))
        .collect();

    let mut current_jobs_by_builder: HashMap<String, Vec<CurrentJob>> = HashMap::new();
    for j in &running_jobs {
        let Some(builder_id) = j.job.builder_id else {
            continue;
        };
        let Some(name) = id_to_name.get(&builder_id.get()).cloned() else {
            continue;
        };
        let live_phase = phases.get(&(name.clone(), j.job.id.get())).copied();
        let (phase, phase_class) = match live_phase {
            Some(argunix_builders::BuildPhase::Push) => ("push", "bg-warn-soft text-warn-strong"),
            Some(argunix_builders::BuildPhase::Build) => ("build", "bg-info-soft text-info-strong"),
            Some(argunix_builders::BuildPhase::Pull) => ("pull", "bg-ok-soft text-ok-strong"),
            None => ("", ""),
        };
        current_jobs_by_builder
            .entry(name)
            .or_default()
            .push(CurrentJob {
                attr_path: j.job.attr_path.to_string(),
                eval_id: j.job.eval_id.get(),
                forge: j.forge.clone(),
                slug: j.slug.as_str().to_string(),
                phase,
                phase_class,
            });
    }

    let now = chrono::Utc::now();
    Ok(collect_builders(
        &roster,
        &live,
        current_jobs_by_builder,
        now,
    ))
}

/// `GET /api/hosts` — same per-builder data as the `/hosts` page, JSON.
/// Returned as one array so polling clients (the page itself, future
/// status-strip live updates) avoid N+1'ing per-builder endpoints just
/// to refresh status / current-job state. The coordinator's stats are
/// served separately by [`host_stats`] to match the per-builder
/// `/api/builders/{name}/stats` shape the page already polls.
pub async fn hosts_json(State(state): State<AppState>) -> Result<Response, UiError> {
    let view = collect_hosts_only(&state).await?;
    let body: Vec<_> = view
        .rows
        .into_iter()
        .map(|b| {
            serde_json::json!({
                "name": b.name,
                "status": b.status,
                "is_online": b.is_online,
                "in_flight": b.in_flight,
                "max_jobs": b.max_jobs,
                "systems": b.systems,
                "features": b.features,
                "nix_version": b.nix_version,
                "last_seen": b.last_seen,
                "current_jobs": b.current_jobs.iter().map(|j| serde_json::json!({
                    "attr_path": j.attr_path,
                    "eval_id": j.eval_id,
                    "forge": j.forge,
                    "slug": j.slug,
                    "phase": j.phase,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        serde_json::to_vec(&serde_json::json!({
            "online": view.online,
            "known": view.known,
            "builders": body,
        }))
        .map_err(UiError::Json)?,
    )
        .into_response())
}

/// Polled htmx fragment for the status page: re-renders just the
/// section content (totals / builders / evaluating / running /
/// queued) so the wrapper div in `status.html` can swap it in every
/// 5s without a full navigation. Returns the same data the full-page
/// handler would, minus the page chrome.
pub async fn status_fragment(State(state): State<AppState>) -> Result<Html<String>, UiError> {
    let view = collect_status_view(&state).await?;
    let cluster_active = !view.evaluating.is_empty() || !view.running.is_empty();
    Ok(Html(render(&StatusInnerTemplate {
        cluster_active,
        totals: view.totals,
        builders: view.builders,
        evaluating: view.evaluating,
        eval_queue_depth: view.eval_queue_depth,
        upcoming_evals: view.upcoming_evals,
        running: view.running,
        queued: view.queued,
        queued_shown: view.queued_shown,
        queued_truncated: view.queued_truncated,
    })?))
}

/// Bag of every row + total the status page shows. Built once per
/// request by [`collect_status_view`]; the full-page and fragment
/// handlers both consume it so the queries — and the truncation /
/// limit logic — stay defined in one place.
struct StatusView {
    totals: ClusterTotals,
    builders: Vec<BuilderRow>,
    evaluating: Vec<EvalRow2>,
    eval_queue_depth: usize,
    upcoming_evals: Vec<EvalRow2>,
    running: Vec<RunningRow>,
    queued: Vec<QueuedRow>,
    queued_shown: usize,
    queued_truncated: bool,
}

/// Result of [`collect_builders`]: the per-builder rows (offline + online),
/// plus a few aggregates the `/hosts` page surfaces in its header
/// summary.
struct HostsView {
    rows: Vec<BuilderRow>,
    online: usize,
    known: usize,
}

/// Build the canonical list of [`BuilderRow`]s. Driven by the persistent
/// roster (so offline / revoked rows still appear) and overlaid with the
/// live registry (so capabilities + in-flight + current-job phases come
/// from the running daemon, not from a stale snapshot in the DB).
///
/// `current_jobs_by_builder` is consumed: each builder's entry is moved
/// out of the map so a caller can detect leftover entries (jobs whose
/// builder name doesn't match any roster row — should never happen, but
/// the contract is "an empty map after this call").
fn collect_builders(
    roster: &[argunix_store::BuilderRecord],
    live: &[argunix_builders::BuilderSnapshot],
    mut current_jobs_by_builder: HashMap<String, Vec<CurrentJob>>,
    now: chrono::DateTime<chrono::Utc>,
) -> HostsView {
    let live_by_name: HashMap<String, &argunix_builders::BuilderSnapshot> = live
        .iter()
        .map(|s| (s.name.as_str().to_string(), s))
        .collect();

    let online = live.iter().filter(|b| b.state == ConnState::Active).count();
    let known = roster.len();

    let rows: Vec<BuilderRow> = roster
        .iter()
        .map(|row| {
            let live = live_by_name.get(row.name.as_str()).copied();
            let current_jobs = current_jobs_by_builder
                .remove(row.name.as_str())
                .unwrap_or_default();
            build_builder_row(row, live, now, current_jobs)
        })
        .collect();

    HostsView {
        rows,
        online,
        known,
    }
}

async fn collect_status_view(state: &AppState) -> Result<StatusView, UiError> {
    // Persistent roster: every builder ever enrolled, including offline
    // / revoked ones. Live registry is the runtime overlay.
    let roster = <argunix_store::SqlxStore as BuilderStore>::list_all(&state.store).await?;
    let live = state.builder_registry.list();

    // We need the running rows before building BuilderRows so each card
    // can carry its `current_jobs`. Run the running-jobs query first,
    // bucket by builder name, then call collect_builders.

    // BuilderId → display name map so running rows can show the operator
    // name rather than the opaque numeric id stored on the job row.
    let builder_id_to_name: HashMap<i64, String> = roster
        .iter()
        .map(|r| (r.id.get(), r.name.as_str().to_string()))
        .collect();

    // Snapshot of live transport/build phases per (builder name,
    // job id). Read once so the running-row loop doesn't take the
    // registry's phase mutex per iteration.
    let phases = state.builder_registry.phase_snapshot();
    let running_jobs = <argunix_store::SqlxStore as JobStore>::list_running(&state.store).await?;
    let running: Vec<RunningRow> = running_jobs
        .into_iter()
        .map(|j| {
            let builder = j
                .job
                .builder_id
                .and_then(|id| builder_id_to_name.get(&id.get()).cloned())
                .unwrap_or_else(|| "—".to_string());
            let live_phase = phases.get(&(builder.clone(), j.job.id.get())).copied();
            let (phase, phase_class) = match live_phase {
                Some(argunix_builders::BuildPhase::Push) => {
                    ("push", "bg-warn-soft text-warn-strong")
                }
                Some(argunix_builders::BuildPhase::Build) => {
                    ("build", "bg-info-soft text-info-strong")
                }
                Some(argunix_builders::BuildPhase::Pull) => ("pull", "bg-ok-soft text-ok-strong"),
                None => ("", ""),
            };
            RunningRow {
                forge: j.forge,
                slug: j.slug.as_str().to_string(),
                eval_id: j.job.eval_id.get(),
                attr_path: j.job.attr_path.to_string(),
                system: j.job.system,
                git_ref: j.git_ref,
                short_sha: j.short_sha,
                builder,
                started: fmt_opt_time(j.job.started_at),
                phase,
                phase_class,
            }
        })
        .collect();

    // Group running rows by their dispatched builder so each builder
    // card can render "now building <attr_path>" without re-walking the
    // running list. Skip the placeholder `—` (jobs without a recorded
    // builder, e.g. local-fallback path).
    let mut current_jobs_by_builder: HashMap<String, Vec<CurrentJob>> = HashMap::new();
    for r in &running {
        if r.builder == "—" {
            continue;
        }
        current_jobs_by_builder
            .entry(r.builder.clone())
            .or_default()
            .push(CurrentJob {
                attr_path: r.attr_path.clone(),
                eval_id: r.eval_id,
                forge: r.forge.clone(),
                slug: r.slug.clone(),
                phase: r.phase,
                phase_class: r.phase_class,
            });
    }

    let now = chrono::Utc::now();
    let builders_view = collect_builders(&roster, &live, current_jobs_by_builder, now);
    let builders = builders_view.rows;
    let builders_online = builders_view.online;

    // Pull `LIMIT + 1` so we can tell whether the queue extends past the
    // display cap without an extra COUNT round-trip.
    let queued_jobs =
        <argunix_store::SqlxStore as JobStore>::list_queued(&state.store, QUEUED_DISPLAY_LIMIT + 1)
            .await?;
    let queued_truncated = queued_jobs.len() as u32 > QUEUED_DISPLAY_LIMIT;
    let queued_total = queued_jobs.len();
    let queued: Vec<QueuedRow> = queued_jobs
        .into_iter()
        .take(QUEUED_DISPLAY_LIMIT as usize)
        .map(|j| QueuedRow {
            forge: j.forge,
            slug: j.slug.as_str().to_string(),
            eval_id: j.job.eval_id.get(),
            attr_path: j.job.attr_path.to_string(),
            system: j.job.system,
            git_ref: j.git_ref,
            short_sha: j.short_sha,
        })
        .collect();
    let queued_shown = queued.len();

    // Surface the worker's eval pipeline. Evaluations are
    // processed serially through a single mpsc → single tokio task,
    // so `evaluating` is normally 0 or 1 rows. >1 only if a previous
    // worker died mid-eval and left a stale row — that's worth
    // showing too. `eval_queue_depth` is the count of `Queued` evals
    // waiting their turn, so an operator can see "3 PRs landed at
    // once, mine is 3rd."
    let evaluating_rows = <argunix_store::SqlxStore as EvalStore>::list_by_status(
        &state.store,
        EvalStatus::Evaluating,
        16,
    )
    .await?;
    let evaluating: Vec<EvalRow2> = evaluating_rows
        .into_iter()
        .map(|r| EvalRow2 {
            eval_id: r.eval.id.get(),
            forge: r.forge,
            slug: r.slug.as_str().to_string(),
            git_ref: r.eval.git_ref,
            short_sha: r.eval.sha.short().to_string(),
            trigger: r.eval.trigger,
            started: fmt_opt_time(r.eval.started_at),
        })
        .collect();
    // Render up to UPCOMING_EVALS_LIMIT queued evals so operators
    // can see *what* is waiting, not just a count. Beyond the limit
    // the template surfaces "N of M" copy. We fetch LIMIT+1 to know
    // whether truncation occurred without an extra COUNT(*) query —
    // though we still call list_queued_ids for the authoritative
    // depth so the header count matches the queue exactly even if
    // a row terminated between the two reads.
    let upcoming_rows = <argunix_store::SqlxStore as EvalStore>::list_by_status(
        &state.store,
        EvalStatus::Queued,
        UPCOMING_EVALS_LIMIT,
    )
    .await?;
    let upcoming_evals: Vec<EvalRow2> = upcoming_rows
        .into_iter()
        .map(|r| EvalRow2 {
            eval_id: r.eval.id.get(),
            forge: r.forge,
            slug: r.slug.as_str().to_string(),
            git_ref: r.eval.git_ref,
            short_sha: r.eval.sha.short().to_string(),
            trigger: r.eval.trigger,
            // Queued rows have no `started_at` yet — fmt_opt_time
            // renders "—". The eval id is monotonic, so older waiting
            // evals naturally sort first.
            started: fmt_opt_time(r.eval.started_at),
        })
        .collect();
    let eval_queue_depth = <argunix_store::SqlxStore as EvalStore>::list_queued_ids(&state.store)
        .await?
        .len();

    let totals = ClusterTotals {
        builders_online,
        builders_known: roster.len(),
        running: running.len(),
        // queued_jobs was capped at LIMIT+1, so we report ≥ truthfully.
        // For >LIMIT queues we show "LIMIT+" using the truncated flag.
        queued_total,
    };

    Ok(StatusView {
        totals,
        builders,
        evaluating,
        eval_queue_depth,
        upcoming_evals,
        running,
        queued,
        queued_shown,
        queued_truncated,
    })
}

fn build_builder_row(
    row: &argunix_store::BuilderRecord,
    live: Option<&argunix_builders::BuilderSnapshot>,
    now: chrono::DateTime<chrono::Utc>,
    current_jobs: Vec<CurrentJob>,
) -> BuilderRow {
    // The live registry is the authoritative source for in_flight and
    // for whether this builder is *currently* online. The persistent
    // row is the source for capabilities snapshot, last_seen, and
    // revocation. We prefer the live capabilities when available since
    // the agent might have re-reported with new `max_jobs`/features
    // before the row was overwritten on the next reconnect.
    let (status, status_class, is_online, in_flight, caps) = if row.revoked_at.is_some() {
        (
            "revoked",
            "bg-fail-soft text-fail-strong",
            false,
            0u32,
            &row.capabilities,
        )
    } else if let Some(snap) = live {
        match snap.state {
            ConnState::Active => (
                "online",
                "bg-ok-soft text-ok-strong",
                true,
                snap.in_flight,
                &snap.capabilities,
            ),
            ConnState::Disconnecting => (
                "draining",
                "bg-warn-soft text-warn-strong",
                true,
                snap.in_flight,
                &snap.capabilities,
            ),
        }
    } else {
        (
            "offline",
            "bg-chip text-chip-fg",
            false,
            0u32,
            &row.capabilities,
        )
    };

    BuilderRow {
        name: row.name.as_str().to_string(),
        status,
        status_class,
        is_online,
        in_flight,
        max_jobs: caps.max_jobs,
        systems: caps.systems.clone(),
        features: caps.features.clone(),
        nix_version: caps.nix_version.clone(),
        last_seen: humanize_last_seen(row.last_seen, now),
        // Offline / revoked rows always carry an empty list — the
        // caller passes `current_jobs` from the live running-jobs
        // index, which only matches a builder name that's online.
        current_jobs: if is_online { current_jobs } else { Vec::new() },
    }
}

/// "5s ago" / "2m ago" / "3h ago" / "yesterday" / absolute timestamp for
/// anything older than a day. Goal is at-a-glance — when a builder
/// disappeared 12 minutes ago, "12m ago" is more useful than the UTC
/// timestamp.
fn humanize_last_seen(
    t: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let delta = now.signed_duration_since(t);
    let secs = delta.num_seconds();
    if secs < 0 {
        // Clock skew. Just show the raw stamp.
        return t.format("%Y-%m-%d %H:%M:%S UTC").to_string();
    }
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        t.format("%Y-%m-%d %H:%M UTC").to_string()
    }
}

/// Single catch-all for everything under `/r/<forge>/...`. Parses the
/// trailing path and routes to the right page. The per-job detail
/// endpoint is content-negotiated: `Accept: application/json`
/// returns the raw record + phase metrics as JSON; anything else
/// renders the HTML page.
pub async fn dispatch_repo(
    AxumPath((forge, tail)): AxumPath<(String, String)>,
    headers: axum::http::HeaderMap,
    state: State<AppState>,
) -> Result<Response, UiError> {
    let parsed = parse_repo_tail(&tail).ok_or(UiError::NotFound)?;
    let slug = Slug::new(parsed.slug.to_string()).map_err(|_| UiError::NotFound)?;

    match parsed.kind {
        TailKind::Repo => repo_page(state, forge, slug).await,
        TailKind::Eval(id) => eval_page(state, forge, slug, EvalId::new(id)).await,
        TailKind::Job { eval_id, attr } => {
            if wants_json(&headers) {
                job_json(state, forge, slug, EvalId::new(eval_id), attr.to_string()).await
            } else {
                job_page(state, forge, slug, EvalId::new(eval_id), attr.to_string()).await
            }
        }
        TailKind::Log { eval_id, attr } => {
            log_handler(state, forge, slug, EvalId::new(eval_id), attr.to_string()).await
        }
    }
}

/// True if the client's `Accept` header prefers JSON. Cheap pattern
/// match — we don't honour qvalues, just look for `application/json`
/// in the header. Matches the read-only-by-design API: clients that
/// want JSON say so, everyone else gets HTML.
fn wants_json(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("application/json"))
        .unwrap_or(false)
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedTail<'a> {
    slug: &'a str,
    kind: TailKind<'a>,
}

#[derive(Debug, PartialEq, Eq)]
enum TailKind<'a> {
    Repo,
    Eval(i64),
    Job { eval_id: i64, attr: &'a str },
    Log { eval_id: i64, attr: &'a str },
}

/// Parse the path tail of `/r/<forge>/<tail>` into a slug + page kind.
/// `tail` looks like one of:
///   `org/repo`
///   `org/repo/eval/12`
///   `org/repo/eval/12/job/packages.x86_64-linux.hello`
///   `org/repo/eval/12/job/packages.x86_64-linux.hello/log`
///   (or with a multi-segment slug for gitlab subgroups).
fn parse_repo_tail(tail: &str) -> Option<ParsedTail<'_>> {
    let tail = tail.trim_end_matches('/');
    if tail.is_empty() {
        return None;
    }
    if let Some(eval_marker) = tail.find("/eval/") {
        let slug = &tail[..eval_marker];
        let after_eval = &tail[eval_marker + "/eval/".len()..];
        let (eval_str, rest) = match after_eval.find('/') {
            Some(i) => (&after_eval[..i], Some(&after_eval[i + 1..])),
            None => (after_eval, None),
        };
        let eval_id: i64 = eval_str.parse().ok()?;
        match rest {
            None => Some(ParsedTail {
                slug,
                kind: TailKind::Eval(eval_id),
            }),
            Some(after) => {
                let after = after.strip_prefix("job/")?;
                if let Some(attr) = after.strip_suffix("/log") {
                    Some(ParsedTail {
                        slug,
                        kind: TailKind::Log { eval_id, attr },
                    })
                } else {
                    Some(ParsedTail {
                        slug,
                        kind: TailKind::Job {
                            eval_id,
                            attr: after,
                        },
                    })
                }
            }
        }
    } else {
        Some(ParsedTail {
            slug: tail,
            kind: TailKind::Repo,
        })
    }
}

async fn repo_page(
    State(state): State<AppState>,
    forge: String,
    slug: Slug,
) -> Result<Response, UiError> {
    let repo = state
        .store
        .find(&forge, &slug)
        .await?
        .ok_or(UiError::NotFound)?;
    let raw_evals = state.store.list_by_repo(repo.id, 50).await?;

    let snap = state.current.load_full();
    let forge_cfg = snap.config.forges.get(&forge);
    let repo_url = repo_url_for(repo.web_url.as_deref(), forge_cfg, slug.as_str());

    // Resolve the branch to surface in the README snippet URL before
    // we consume `raw_evals` for template rendering. Authoritative
    // source is `repo.default_branch`; for pre-migration repos with
    // no webhook yet, fall back to the most-recent push-triggered
    // git_ref, then to "main".
    let snippet_branch = crate::badge::snippet_branch(repo.default_branch.as_deref(), &raw_evals);

    let evals = raw_evals
        .into_iter()
        .map(|e| {
            let (forge_link, commit_link) = forge_links_for_eval(
                &repo_url,
                forge_cfg,
                &e.git_ref,
                e.pr_number,
                e.sha.as_str(),
            );
            EvalRow {
                id: e.id.get(),
                git_ref: display_git_ref(&e.git_ref),
                short_sha: short_sha(e.sha.as_str()).to_string(),
                status: eval_status_label(&e.status),
                finished: fmt_opt_time(e.finished_at),
                total: humanize_duration(e.started_at, e.finished_at),
                pr_number: e.pr_number,
                forge_link,
                commit_link,
            }
        })
        .collect();

    let cluster_active = cluster_is_active(&state).await?;
    let external_url = &snap.config.external_url;
    // The snippet URL always carries an explicit `/<branch>.svg`
    // segment — the badge endpoint reads it and filters by it (see
    // `badge::handle`), and showing the branch in the rendered
    // markdown signals to README readers that the segment is
    // editable.
    let snippet_branch_ref = Some(snippet_branch.as_str());
    let badge_url =
        crate::badge::badge_url(external_url, &forge, slug.as_str(), snippet_branch_ref);
    let badge_markdown =
        crate::badge::markdown_snippet(external_url, &forge, slug.as_str(), snippet_branch_ref);
    let html = render(&RepoTemplate {
        cluster_active,
        forge,
        slug: slug.as_str().to_string(),
        name: repo.name,
        description: repo.description,
        repo_url,
        badge_url,
        badge_markdown,
        evals,
    })?;
    Ok(Html(html).into_response())
}

/// Normalise a stored `git_ref` for display. Push-triggered evals
/// already store the short branch name post-ingest. PR-triggered
/// evals store a synthetic `refs/pull/<n>/head:<headref>` form for
/// branch-key matching; surface only the trailing `<headref>` part
/// so the cell reads as just the source branch name.
fn display_git_ref(git_ref: &str) -> String {
    git_ref
        .rsplit_once(':')
        .map(|(_, branch)| branch)
        .unwrap_or(git_ref)
        .to_string()
}

/// Build `(branch_or_pr_url, commit_url)` for one eval row. Both are
/// empty strings when no forge URL can be constructed.
///
/// The branch link uses the *short* form of `git_ref` — push refs are
/// already stored that way (M-refs-normalize); PR-triggered evals
/// store a synthetic `refs/pull/N/head:<branch>` shape and link to
/// the PR via `pr_number` instead, ignoring `git_ref`.
fn forge_links_for_eval(
    repo_url: &str,
    forge_cfg: Option<&ForgeConfig>,
    git_ref: &str,
    pr_number: Option<u32>,
    sha: &str,
) -> (String, String) {
    let Some(cfg) = forge_cfg else {
        return (String::new(), String::new());
    };
    if repo_url.is_empty() {
        return (String::new(), String::new());
    }
    let primary = match pr_number {
        Some(n) => cfg.kind.pr_url(repo_url, n),
        None => cfg.kind.branch_url(repo_url, git_ref),
    };
    let commit = cfg.kind.commit_url(repo_url, sha);
    (primary, commit)
}

async fn eval_page(
    State(state): State<AppState>,
    forge: String,
    slug: Slug,
    eval_id: EvalId,
) -> Result<Response, UiError> {
    let eval = <argunix_store::SqlxStore as EvalStore>::get(&state.store, eval_id)
        .await?
        .ok_or(UiError::NotFound)?;
    let jobs = <argunix_store::SqlxStore as JobStore>::list_by_eval(&state.store, eval_id).await?;

    let job_heading = job_heading_for(&eval.status, jobs.len());
    let empty_jobs_msg = empty_jobs_msg_for(&eval.status);
    let job_rows = jobs
        .into_iter()
        .map(|j| JobRow {
            attr_path: j.attr_path.to_string(),
            system: j.system,
            status: job_status_label(&j.status),
            glyph: job_status_glyph(&j.status),
            glyph_class: job_status_glyph_class(&j.status),
            finished: fmt_opt_time(j.finished_at),
            has_log: j.log_path.is_some(),
            duration: humanize_duration(j.started_at, j.finished_at),
        })
        .collect();

    let snap = state.current.load_full();
    let forge_cfg = snap.config.forges.get(&forge);
    let repo = state.store.find(&forge, &slug).await?;
    let repo_web_url = repo.as_ref().and_then(|r| r.web_url.as_deref());
    let repo_url = repo_url_for(repo_web_url, forge_cfg, slug.as_str());
    let (ref_link, commit_link) = forge_links_for_eval(
        &repo_url,
        forge_cfg,
        &eval.git_ref,
        eval.pr_number,
        eval.sha.as_str(),
    );

    let cluster_active = cluster_is_active(&state).await?;
    let html = render(&EvalTemplate {
        cluster_active,
        forge,
        slug: slug.as_str().to_string(),
        eval_id: eval_id.get(),
        status_label: eval_status_label(&eval.status),
        phase_class: eval_status_phase_class(&eval.status),
        trigger: eval.trigger.to_string(),
        git_ref: display_git_ref(&eval.git_ref),
        sha: eval.sha.to_string(),
        started: fmt_opt_time(eval.started_at),
        finished: fmt_opt_time(eval.finished_at),
        total: humanize_duration(eval.started_at, eval.finished_at),
        eval_time: humanize_duration(eval.started_at, eval.building_started_at),
        build_time: humanize_duration(eval.building_started_at, eval.finished_at),
        job_heading,
        empty_jobs_msg,
        jobs: job_rows,
        pr_number: eval.pr_number,
        failure_reason: eval.failure_reason,
        repo_url,
        ref_link,
        commit_link,
    })?;
    Ok(Html(html).into_response())
}

async fn job_page(
    State(state): State<AppState>,
    forge: String,
    slug: Slug,
    eval_id: EvalId,
    attr: String,
) -> Result<Response, UiError> {
    let jobs = <argunix_store::SqlxStore as JobStore>::list_by_eval(&state.store, eval_id).await?;
    let job = jobs
        .into_iter()
        .find(|j| j.attr_path.as_str() == attr)
        .ok_or(UiError::NotFound)?;

    let pm = job.phase_metrics;
    let phase_metrics = PhaseMetricsRow {
        has_any: pm.push_bytes.is_some()
            || pm.push_ms.is_some()
            || pm.build_ms.is_some()
            || pm.pull_bytes.is_some()
            || pm.pull_ms.is_some(),
        push_bytes: humanize_bytes(pm.push_bytes),
        push_ms: humanize_ms(pm.push_ms),
        build_ms: humanize_ms(pm.build_ms),
        pull_bytes: humanize_bytes(pm.pull_bytes),
        pull_ms: humanize_ms(pm.pull_ms),
    };

    let live_builder = if matches!(job.status, JobStatus::Running) {
        state
            .builder_registry
            .builder_for_build(job.id.get())
            .map(|n| n.as_str().to_string())
    } else {
        None
    };

    let cluster_active = cluster_is_active(&state).await?;
    let html = render(&JobTemplate {
        cluster_active,
        forge,
        slug: slug.as_str().to_string(),
        eval_id: eval_id.get(),
        job_id: job.id.get(),
        attr_path: job.attr_path.to_string(),
        system: job.system,
        status_label: job_status_label(&job.status),
        started: fmt_opt_time(job.started_at),
        finished: fmt_opt_time(job.finished_at),
        total: humanize_duration(job.started_at, job.finished_at),
        drv_path: job.drv_path,
        output_path: job.output_path,
        has_log: job.log_path.is_some(),
        live_builder,
        phase_metrics,
    })?;
    Ok(Html(html).into_response())
}

/// JSON shape for the per-job detail endpoint. Returned when the
/// client sends `Accept: application/json` to the job route. The
/// shape is the JobRecord's UI-relevant subset plus the per-phase
/// transport accounting — bytes through our russh tunnel and
/// wall-clock per phase, all `null` when the job wasn't dispatched
/// to the pool. Pretty-printed for terminal-friendly `curl | jq`.
async fn job_json(
    State(state): State<AppState>,
    forge: String,
    slug: Slug,
    eval_id: EvalId,
    attr: String,
) -> Result<Response, UiError> {
    let jobs = <argunix_store::SqlxStore as JobStore>::list_by_eval(&state.store, eval_id).await?;
    let job = jobs
        .into_iter()
        .find(|j| j.attr_path.as_str() == attr)
        .ok_or(UiError::NotFound)?;

    let body = serde_json::json!({
        "forge": forge,
        "slug": slug.as_str(),
        "eval_id": eval_id.get(),
        "attr_path": job.attr_path.to_string(),
        "system": job.system,
        "status": job_status_label(&job.status),
        "started_at": job.started_at.map(|t| t.to_rfc3339()),
        "finished_at": job.finished_at.map(|t| t.to_rfc3339()),
        "drv_path": job.drv_path,
        "output_path": job.output_path,
        "log_path": job.log_path,
        // Per-phase transport accounting. `null` for jobs that
        // were built locally (no remote-transport phases).
        "phase_metrics": {
            "push_bytes": job.phase_metrics.push_bytes,
            "push_ms": job.phase_metrics.push_ms,
            "build_ms": job.phase_metrics.build_ms,
            "pull_bytes": job.phase_metrics.pull_bytes,
            "pull_ms": job.phase_metrics.pull_ms,
        },
    });
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        serde_json::to_vec_pretty(&body).map_err(UiError::Json)?,
    )
        .into_response())
}

/// True iff anything is currently evaluating or running. Computed
/// once per page render and passed to every template's `cluster_active`
/// field; the base layout reads it to decide whether the header logo
/// gets the `argunix-spin` class. Two `LIMIT 1`-sized queries.
async fn cluster_is_active(state: &AppState) -> Result<bool, UiError> {
    let evaluating = <argunix_store::SqlxStore as EvalStore>::list_by_status(
        &state.store,
        EvalStatus::Evaluating,
        1,
    )
    .await?;
    if !evaluating.is_empty() {
        return Ok(true);
    }
    let running = <argunix_store::SqlxStore as JobStore>::list_running(&state.store).await?;
    Ok(!running.is_empty())
}

/// `GET /api/host/stats` — JSON ring of recent stats samples for the
/// argunix coordinator host (cpu / load1 / mem). Same shape as
/// [`builder_stats`] so the `/hosts` page can reuse the same fetch +
/// sparkline JS for both card kinds. Empty list (200) until the
/// background sampler has produced its first sample (~5s after boot).
pub async fn host_stats(State(state): State<AppState>) -> Result<Response, UiError> {
    let body: Vec<_> = state
        .host_stats
        .snapshot()
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "ts": s.ts.to_rfc3339(),
                "load1": s.stats.load1,
                "cpu_percent": s.stats.cpu_percent,
                "mem_used_bytes": s.stats.mem_used_bytes,
                "mem_total_bytes": s.stats.mem_total_bytes,
            })
        })
        .collect();
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        serde_json::to_vec(&body).map_err(UiError::Json)?,
    )
        .into_response())
}

/// `GET /api/builders/{name}/stats` — JSON ring of recent heartbeat
/// stats samples for one connected builder. Polled every ~5s by the
/// job page's sparkline JS. Returns an empty list (200) for a
/// connected builder that hasn't sent a stats-bearing heartbeat yet,
/// 404 only when the builder name is invalid.
pub async fn builder_stats(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Result<Response, UiError> {
    let name = argunix_domain::BuilderName::new(&name).map_err(|_| UiError::NotFound)?;
    let samples = state.builder_registry.stats_snapshot(&name);
    let body: Vec<_> = samples
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "ts": s.ts.to_rfc3339(),
                "load1": s.stats.load1,
                "cpu_percent": s.stats.cpu_percent,
                "mem_used_bytes": s.stats.mem_used_bytes,
                "mem_total_bytes": s.stats.mem_total_bytes,
            })
        })
        .collect();
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        serde_json::to_vec(&body).map_err(UiError::Json)?,
    )
        .into_response())
}

/// `GET /api/jobs/{job_id}/log/stream` — SSE tail of a running build's
/// stderr. Replays the buffered prefix as a single `data:` event so a
/// late-joining browser sees the full log so far, then forwards each
/// new chunk from the broadcast tap until the build finishes (the
/// registry entry is dropped → broadcast receiver closes → stream
/// ends naturally). 404 if no entry — caller should fall back to the
/// static `/log` endpoint.
pub async fn job_log_stream(
    State(state): State<AppState>,
    AxumPath(job_id): AxumPath<i64>,
) -> Result<Response, UiError> {
    let live = state.live_logs.get(job_id).ok_or(UiError::NotFound)?;
    let (initial, mut rx) = live.subscribe();

    use axum::response::sse::{Event, KeepAlive, Sse};
    use tokio_stream::wrappers::ReceiverStream;

    let (tx, out_rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

    if !initial.is_empty() {
        let _ = tx
            .send(Ok(Event::default().data(String::from_utf8_lossy(&initial))))
            .await;
    }
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    if tx
                        .send(Ok(Event::default().data(String::from_utf8_lossy(&bytes))))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // Lagged: tell the client we lost some bytes rather
                // than silently skipping. The buffer is unbounded
                // server-side; lag here is purely the broadcast
                // channel's depth, hit only by very slow clients.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let _ = tx
                        .send(Ok(Event::default()
                            .event("lag")
                            .data(format!("dropped {n} chunks"))))
                        .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    let _ = tx.send(Ok(Event::default().event("end").data(""))).await;
                    break;
                }
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(out_rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// Stream the zstd-compressed build log decompressed as `text/plain`.
async fn log_handler(
    State(state): State<AppState>,
    _forge: String,
    _slug: Slug,
    eval_id: EvalId,
    attr: String,
) -> Result<Response, UiError> {
    let jobs = <argunix_store::SqlxStore as JobStore>::list_by_eval(&state.store, eval_id).await?;
    let job = jobs
        .into_iter()
        .find(|j| j.attr_path.as_str() == attr)
        .ok_or(UiError::NotFound)?;
    let log_path = job.log_path.as_deref().ok_or(UiError::NotFound)?;

    let path = log_path.to_string();
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        let f = std::fs::File::open(&path)?;
        let mut decoder = zstd::stream::Decoder::new(f)?;
        let mut buf = Vec::new();
        std::io::copy(&mut decoder, &mut buf)?;
        Ok(buf)
    })
    .await
    .map_err(|e| UiError::LogRead(std::io::Error::other(e)))?
    .map_err(UiError::LogRead)?;

    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        bytes,
    )
        .into_response())
}

// -------- helpers --------

fn render<T: Template>(t: &T) -> Result<String, UiError> {
    t.render().map_err(UiError::Render)
}

fn short_sha(sha: &str) -> &str {
    if sha.len() >= 7 { &sha[..7] } else { sha }
}

fn fmt_opt_time(t: Option<chrono::DateTime<chrono::Utc>>) -> String {
    match t {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => "—".to_string(),
    }
}

/// Render a byte count with an SI-ish unit (KB/MB/GB at 1024-base) and
/// the raw byte total in parens. `None` → `"—"`. Used for the per-job
/// page's transport rows. Three sigfigs is plenty — "523 MB" is
/// more useful than "523.418 MB", and the raw value is right next to it
/// for anyone who needs the exact figure.
fn humanize_bytes(b: Option<u64>) -> String {
    let Some(b) = b else { return "—".to_string() };
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Render a millisecond duration as `Hh Mm Ss` / `Mm Ss` / `Ss` /
/// `123 ms`, picking the largest non-zero grain. `None` → `"—"`.
/// Wall-clock between two timestamps, humanized via [`humanize_ms`].
/// `"—"` when either side is missing or the diff would be negative
/// (clock skew on a freshly-cancelled row).
fn humanize_duration(
    started: Option<chrono::DateTime<chrono::Utc>>,
    finished: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    let (Some(s), Some(f)) = (started, finished) else {
        return "—".to_string();
    };
    let ms = (f - s).num_milliseconds();
    if ms < 0 {
        return "—".to_string();
    }
    humanize_ms(Some(ms as u64))
}

fn humanize_ms(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "—".to_string();
    };
    if ms < 1000 {
        return format!("{ms} ms");
    }
    let total_s = ms / 1000;
    let h = total_s / 3600;
    let m = (total_s % 3600) / 60;
    let s = total_s % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn eval_status_label(s: &EvalStatus) -> &'static str {
    match s {
        EvalStatus::Queued => "queued",
        EvalStatus::Evaluating => "evaluating",
        EvalStatus::Building => "building",
        EvalStatus::Done => "done",
        EvalStatus::EvaluationFailed => "evaluation failed",
        EvalStatus::Cancelled => "cancelled",
    }
}

/// CSS class suffix used to colour the eval-status badge — terminal
/// states get a fixed colour, transient states get the "active" hue.
fn eval_status_phase_class(s: &EvalStatus) -> &'static str {
    match s {
        EvalStatus::Queued | EvalStatus::Evaluating | EvalStatus::Building => "active",
        EvalStatus::Done => "ok",
        EvalStatus::EvaluationFailed => "fail",
        EvalStatus::Cancelled => "muted",
    }
}

/// Header for the per-eval jobs section. Tells the user whether the
/// list they're looking at is final ("jobs (N)") or still in progress
/// ("jobs (N so far)" while building, etc.).
fn job_heading_for(status: &EvalStatus, count: usize) -> String {
    match status {
        EvalStatus::Queued | EvalStatus::Evaluating => {
            "jobs (eval still running — list not yet known)".to_string()
        }
        EvalStatus::Building => format!("jobs ({count}, all discovered)"),
        EvalStatus::Done | EvalStatus::EvaluationFailed | EvalStatus::Cancelled => {
            format!("jobs ({count})")
        }
    }
}

fn empty_jobs_msg_for(status: &EvalStatus) -> &'static str {
    match status {
        EvalStatus::Queued => "Queued — waiting for the worker to pick this up.",
        EvalStatus::Evaluating => {
            "Evaluating the flake. Job list will appear when nix-eval-jobs finishes."
        }
        EvalStatus::EvaluationFailed => "Evaluation failed before any jobs were discovered.",
        EvalStatus::Cancelled => "Cancelled before any jobs were discovered.",
        _ => "No jobs recorded for this evaluation.",
    }
}

fn job_status_label(s: &JobStatus) -> &'static str {
    match s {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Cached => "cached",
        JobStatus::Success => "success",
        JobStatus::Failure => "failure",
        JobStatus::Cancelled => "cancelled",
        JobStatus::Interrupted => "interrupted",
        JobStatus::SkippedNoBuilder => "skipped",
    }
}

/// Single Unicode glyph used in the eval-page jobs table in place of
/// the textual status. Picked so each bucket is *shape*-distinct (not
/// just colour-distinct) — a colourblind reader can still tell ✓
/// apart from ✗ without seeing the green/red.
fn job_status_glyph(s: &JobStatus) -> &'static str {
    match s {
        JobStatus::Success | JobStatus::Cached => "✓",
        JobStatus::Failure => "✗",
        JobStatus::Interrupted => "⚠",
        JobStatus::Cancelled | JobStatus::SkippedNoBuilder => "⊘",
        JobStatus::Running => "⋯",
        JobStatus::Queued => "○",
    }
}

/// Tailwind colour class for the glyph wrapper. Mirrors the ok / fail
/// / info / warn / muted buckets used by the eval-status pill so the
/// palette stays consistent across pages.
fn job_status_glyph_class(s: &JobStatus) -> &'static str {
    match s {
        JobStatus::Success | JobStatus::Cached => "text-ok-strong",
        JobStatus::Failure => "text-fail-strong",
        JobStatus::Interrupted => "text-warn-strong",
        JobStatus::Running => "text-info-strong",
        JobStatus::Queued | JobStatus::Cancelled | JobStatus::SkippedNoBuilder => "text-muted",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error("not found")]
    NotFound,
    #[error("store: {0}")]
    Store(#[from] argunix_store::StoreError),
    #[error("reading build log: {0}")]
    LogRead(#[source] std::io::Error),
    #[error("rendering template: {0}")]
    Render(#[source] askama::Error),
    #[error("encoding JSON response: {0}")]
    Json(#[source] serde_json::Error),
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        let status = match &self {
            UiError::NotFound => StatusCode::NOT_FOUND,
            UiError::Store(_) | UiError::LogRead(_) | UiError::Render(_) | UiError::Json(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        if status.is_server_error() {
            tracing::error!(error = %self, "ui handler failed");
        }
        let body = self.to_string();
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_template_renders_empty_cluster() {
        // Smoke: no builders, nothing running, nothing queued. Verifies
        // the template's empty-state branches all compile + render,
        // including the included `_status_inner.html` partial.
        let tmpl = StatusTemplate {
            cluster_active: false,
            totals: ClusterTotals {
                builders_online: 0,
                builders_known: 0,
                running: 0,
                queued_total: 0,
            },
            builders: vec![],
            evaluating: vec![],
            eval_queue_depth: 0,
            upcoming_evals: vec![],
            running: vec![],
            queued: vec![],
            queued_shown: 0,
            queued_truncated: false,
        };
        let html = tmpl.render().unwrap();
        assert!(html.contains("cluster status"));
        assert!(html.contains("No builders enrolled yet"));
        assert!(html.contains("Nothing is building"));
        assert!(html.contains("Queue is empty"));
        // The wrapper div is what htmx polls — without it the page
        // would silently regress to a static snapshot.
        assert!(html.contains(r#"hx-get="/_/status""#));
        assert!(html.contains(r#"hx-trigger="every 5s""#));
    }

    #[test]
    fn status_inner_template_renders_empty_cluster() {
        // The fragment endpoint must produce the same section content
        // as the include path on first load — guard that the include
        // and the standalone render stay in sync.
        let tmpl = StatusInnerTemplate {
            cluster_active: false,
            totals: ClusterTotals {
                builders_online: 0,
                builders_known: 0,
                running: 0,
                queued_total: 0,
            },
            builders: vec![],
            evaluating: vec![],
            eval_queue_depth: 0,
            upcoming_evals: vec![],
            running: vec![],
            queued: vec![],
            queued_shown: 0,
            queued_truncated: false,
        };
        let html = tmpl.render().unwrap();
        assert!(html.contains("No builders enrolled yet"));
        assert!(html.contains("Nothing is building"));
        assert!(html.contains("Queue is empty"));
        // The fragment is *only* the inner content + the OOB logo
        // swap — no <h1>, no polling wrapper. If the page heading or
        // the wrapper leak in, the polling swap would inject a
        // duplicate every 5s.
        assert!(!html.contains("cluster status"));
        assert!(!html.contains("hx-get"));
        // Logo OOB swap is what keeps the header spinner in sync.
        assert!(html.contains(r#"id="argunix-logo""#));
        assert!(html.contains(r#"hx-swap-oob="true""#));
        // Idle cluster → no spin class on the OOB image.
        assert!(!html.contains("argunix-spin"));
    }

    #[test]
    fn status_inner_template_marks_logo_active_when_running() {
        // One running job is enough to flip the spinner on.
        let tmpl = StatusInnerTemplate {
            cluster_active: true,
            totals: ClusterTotals {
                builders_online: 0,
                builders_known: 0,
                running: 1,
                queued_total: 0,
            },
            builders: vec![],
            evaluating: vec![],
            eval_queue_depth: 0,
            upcoming_evals: vec![],
            running: vec![RunningRow {
                forge: "github".into(),
                slug: "owner/repo".into(),
                eval_id: 1,
                attr_path: "packages.x86_64-linux.hello".into(),
                system: "x86_64-linux".into(),
                git_ref: "main".into(),
                short_sha: "abcdef0".into(),
                builder: "—".into(),
                started: "—".into(),
                phase: "",
                phase_class: "",
            }],
            queued: vec![],
            queued_shown: 0,
            queued_truncated: false,
        };
        let html = tmpl.render().unwrap();
        assert!(html.contains("argunix-spin"));
    }

    fn make_builder_row(name: &str, online: bool, jobs: Vec<CurrentJob>) -> BuilderRow {
        BuilderRow {
            name: name.into(),
            status: if online { "online" } else { "offline" },
            status_class: if online {
                "bg-ok-soft text-ok-strong"
            } else {
                "bg-chip text-chip-fg"
            },
            is_online: online,
            in_flight: jobs.len() as u32,
            max_jobs: 4,
            systems: vec!["x86_64-linux".into()],
            features: vec!["big-parallel".into()],
            nix_version: "2.18.1".into(),
            last_seen: "5m ago".into(),
            current_jobs: jobs,
        }
    }

    fn fixture_coordinator() -> CoordinatorRow {
        CoordinatorRow {
            hostname: "argunix-test".into(),
            uptime: "1m 20s".into(),
            nix_version: "2.24.10".into(),
            nix_eval_jobs_version: "2.24.0".into(),
        }
    }

    #[test]
    fn hosts_template_empty_state_mentions_enrollment_help() {
        let html = HostsTemplate {
            cluster_active: false,
            coordinator: fixture_coordinator(),
            rows: vec![],
            online: 0,
            known: 0,
        }
        .render()
        .unwrap();
        // Operator with zero builders gets a hint about how to add one
        // — without it the "no builders" line is a dead-end.
        assert!(html.contains("No builders enrolled yet"));
        assert!(html.contains("services.argunix-builder.enable"));
        // Coordinator card is rendered even with zero builders.
        assert!(html.contains("argunix-test"));
        assert!(html.contains("coordinator"));
    }

    #[test]
    fn hosts_template_renders_busy_card_with_phase_and_link() {
        let job = CurrentJob {
            attr_path: "packages.x86_64-linux.hello".into(),
            eval_id: 42,
            forge: "github".into(),
            slug: "owner/repo".into(),
            phase: "build",
            phase_class: "bg-info-soft text-info-strong",
        };
        let html = HostsTemplate {
            cluster_active: true,
            coordinator: fixture_coordinator(),
            rows: vec![make_builder_row("alpha", true, vec![job])],
            online: 1,
            known: 1,
        }
        .render()
        .unwrap();
        // Card shape + identifying data.
        assert!(html.contains("alpha"));
        assert!(html.contains("packages.x86_64-linux.hello"));
        // Phase badge present so operators can tell push/build/pull
        // at a glance.
        assert!(html.contains(">build</span>"));
        // Link points at the per-job page (not /hosts) so a click
        // drills into the active build, not back to the same view.
        assert!(html.contains("/r/github/owner/repo/eval/42/job/packages.x86_64-linux.hello"));
        // Sparkline JS attaches via [data-online="1"] selector — must
        // be present on busy cards.
        assert!(html.contains(r#"data-online="1""#));
    }

    #[test]
    fn hosts_template_renders_idle_card_without_sparklines_for_offline() {
        let html = HostsTemplate {
            cluster_active: false,
            coordinator: fixture_coordinator(),
            rows: vec![make_builder_row("beta", false, vec![])],
            online: 0,
            known: 1,
        }
        .render()
        .unwrap();
        assert!(html.contains("beta"));
        // Offline builder cards skip the sparkline figure block —
        // drawing sparklines for a builder whose stats endpoint
        // returns [] would just paint a flat line forever. The
        // coordinator card always carries one sparkline set, so we
        // assert exactly one `<svg data-spark="cpu"` (the
        // coordinator's), not zero.
        assert_eq!(html.matches(r#"<svg data-spark="cpu""#).count(), 1);
        assert!(html.contains("last seen 5m ago"));
        assert!(html.contains(r#"data-online="0""#));
    }

    #[test]
    fn status_inner_renders_builder_card_with_now_building_attr_path() {
        // Mirrors the homepage strip path: `_status_inner.html`'s
        // builder section should render the head of `current_jobs`
        // inline so an operator scanning the home page can see what
        // each box is chewing on without clicking through.
        let job = CurrentJob {
            attr_path: "checks.x86_64-linux.smoke".into(),
            eval_id: 7,
            forge: "github".into(),
            slug: "a/b".into(),
            phase: "push",
            phase_class: "bg-warn-soft text-warn-strong",
        };
        let tmpl = StatusInnerTemplate {
            cluster_active: true,
            totals: ClusterTotals {
                builders_online: 1,
                builders_known: 1,
                running: 1,
                queued_total: 0,
            },
            builders: vec![make_builder_row("gamma", true, vec![job])],
            evaluating: vec![],
            eval_queue_depth: 0,
            upcoming_evals: vec![],
            running: vec![],
            queued: vec![],
            queued_shown: 0,
            queued_truncated: false,
        };
        let html = tmpl.render().unwrap();
        assert!(html.contains("checks.x86_64-linux.smoke"));
        assert!(html.contains(">push</span>"));
        // Strip card always links to the dedicated /hosts page so
        // the homepage stays the cluster overview.
        assert!(html.contains(r#"href="/hosts""#));
    }

    #[test]
    fn humanize_last_seen_picks_appropriate_grain() {
        let now = chrono::Utc::now();
        assert!(humanize_last_seen(now - chrono::Duration::seconds(5), now).contains("s ago"));
        assert!(humanize_last_seen(now - chrono::Duration::minutes(7), now).contains("m ago"));
        assert!(humanize_last_seen(now - chrono::Duration::hours(4), now).contains("h ago"));
        // > 1 day → absolute stamp.
        assert!(humanize_last_seen(now - chrono::Duration::days(3), now).contains("UTC"));
    }

    #[test]
    fn parse_repo_only() {
        let p = parse_repo_tail("myorg/myrepo").unwrap();
        assert_eq!(p.slug, "myorg/myrepo");
        assert_eq!(p.kind, TailKind::Repo);
    }

    #[test]
    fn parse_subgroup_slug() {
        let p = parse_repo_tail("org/sub/marketing/repo").unwrap();
        assert_eq!(p.slug, "org/sub/marketing/repo");
        assert_eq!(p.kind, TailKind::Repo);
    }

    #[test]
    fn parse_eval() {
        let p = parse_repo_tail("myorg/myrepo/eval/42").unwrap();
        assert_eq!(p.slug, "myorg/myrepo");
        assert_eq!(p.kind, TailKind::Eval(42));
    }

    #[test]
    fn parse_eval_with_subgroup() {
        let p = parse_repo_tail("org/sub/marketing/repo/eval/7").unwrap();
        assert_eq!(p.slug, "org/sub/marketing/repo");
        assert_eq!(p.kind, TailKind::Eval(7));
    }

    #[test]
    fn parse_job() {
        let p = parse_repo_tail("myorg/myrepo/eval/3/job/packages.x86_64-linux.hello").unwrap();
        assert_eq!(p.slug, "myorg/myrepo");
        assert_eq!(
            p.kind,
            TailKind::Job {
                eval_id: 3,
                attr: "packages.x86_64-linux.hello",
            }
        );
    }

    #[test]
    fn parse_log() {
        let p = parse_repo_tail("myorg/myrepo/eval/3/job/packages.x86_64-linux.hello/log").unwrap();
        assert_eq!(p.slug, "myorg/myrepo");
        assert_eq!(
            p.kind,
            TailKind::Log {
                eval_id: 3,
                attr: "packages.x86_64-linux.hello",
            }
        );
    }

    #[test]
    fn rejects_empty_tail() {
        assert!(parse_repo_tail("").is_none());
    }

    #[test]
    fn rejects_non_numeric_eval() {
        assert!(parse_repo_tail("myorg/myrepo/eval/abc").is_none());
    }

    #[test]
    fn ignores_trailing_slash() {
        let p = parse_repo_tail("myorg/myrepo/").unwrap();
        assert_eq!(p.slug, "myorg/myrepo");
        assert_eq!(p.kind, TailKind::Repo);
    }
}
