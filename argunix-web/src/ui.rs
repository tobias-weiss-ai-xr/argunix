//! Read-only HTML UI (M6-lite).
//!
//! Routes:
//!   GET /                                                       — index, list of repos
//!   GET /r/<forge>/<...slug>                                    — repo page (recent evals)
//!   GET /r/<forge>/<...slug>/eval/<id>                          — eval detail (job table)
//!   GET /r/<forge>/<...slug>/eval/<id>/job/<attr>               — single job detail
//!   GET /r/<forge>/<...slug>/eval/<id>/job/<attr>/log           — decompressed build log
//!
//! All `/r/...` paths share a single axum catch-all (`/r/{forge}/{*tail}`)
//! and dispatch on segment markers (`/eval/`, `/job/`) per Q97 — that's
//! the only way to support gitlab-subgroup slugs that contain slashes
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
    /// Rendered as small pill badges in `status.html`. Stored as
    /// `Vec<String>` rather than a comma-joined string so the template
    /// can iterate.
    systems: Vec<String>,
    features: Vec<String>,
    nix_version: String,
    /// Suppressed in the template when `is_online` is true (we already
    /// know it's live; "last seen" reads as past tense).
    last_seen: String,
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
    /// pool builder (M16). `"push"`, `"build"`, `"pull"`, or empty for
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

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    cluster_active: bool,
    repos: Vec<RepoRow>,
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
    /// Forge-supplied display name. Falls back to `slug` in the
    /// template when `None`.
    name: Option<String>,
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
    /// M16 per-phase transport accounting. Each pair is rendered as
    /// "<value> (<raw>)" already-formatted; absent fields surface as
    /// "—". The whole block is suppressed in the template when no
    /// phase has data, so jobs built locally / pre-M16 stay clean.
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
    let raw = state.store.list().await?;
    let mut repos: Vec<RepoRow> = Vec::with_capacity(raw.len());
    for r in raw {
        // One latest-eval lookup per repo. Cheap (LIMIT 1, indexed); we
        // skip a join here so the query stays trivially correct.
        let latest_eval_id =
            <argunix_store::SqlxStore as EvalStore>::list_by_repo(&state.store, r.id, 1)
                .await?
                .into_iter()
                .next()
                .map(|e| e.id.get());
        let forge_cfg = snap.config.forges.get(&r.forge);
        let forge_url = forge_url_for(forge_cfg, r.slug.as_str());
        let repo_url = repo_url_for(r.web_url.as_deref(), forge_cfg, r.slug.as_str());
        repos.push(RepoRow {
            forge: r.forge,
            forge_url,
            slug: r.slug.as_str().to_string(),
            repo_url,
            name: r.name,
            description: r.description,
            latest_eval_id,
        });
    }
    Ok(Html(render(&IndexTemplate {
        cluster_active,
        repos,
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
fn repo_url_for(repo_web_url: Option<&str>, forge_cfg: Option<&ForgeConfig>, slug: &str) -> String {
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
/// and what the cluster is doing right now. Auto-refreshes via meta tag
/// (see `templates/status.html`).
pub async fn status(State(state): State<AppState>) -> Result<Html<String>, UiError> {
    // Persistent roster: every builder ever enrolled, including offline
    // / revoked ones. Live registry is the runtime overlay.
    let roster = <argunix_store::SqlxStore as BuilderStore>::list_all(&state.store).await?;
    let live = state.builder_registry.list();
    let live_by_name: HashMap<String, &argunix_builders::BuilderSnapshot> = live
        .iter()
        .map(|s| (s.name.as_str().to_string(), s))
        .collect();

    let now = chrono::Utc::now();
    let builders: Vec<BuilderRow> = roster
        .iter()
        .map(|row| build_builder_row(row, live_by_name.get(row.name.as_str()).copied(), now))
        .collect();

    let builders_online = live.iter().filter(|b| b.state == ConnState::Active).count();

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
                Some(argunix_builders::BuildPhase::Push) => ("push", "bg-amber-100 text-amber-700"),
                Some(argunix_builders::BuildPhase::Build) => ("build", "bg-blue-100 text-blue-700"),
                Some(argunix_builders::BuildPhase::Pull) => {
                    ("pull", "bg-emerald-100 text-emerald-700")
                }
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

    // M16: surface the worker's eval pipeline. Evaluations are
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
    // For the queue depth, we don't need the rows themselves — just
    // the count. Cap at LIMIT+1 isn't needed here either (sqlite
    // reads are cheap and we want the actual depth for display).
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

    let cluster_active = !evaluating.is_empty() || !running.is_empty();
    Ok(Html(render(&StatusTemplate {
        cluster_active,
        totals,
        builders,
        evaluating,
        eval_queue_depth,
        running,
        queued,
        queued_shown,
        queued_truncated,
    })?))
}

fn build_builder_row(
    row: &argunix_store::BuilderRecord,
    live: Option<&argunix_builders::BuilderSnapshot>,
    now: chrono::DateTime<chrono::Utc>,
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
            "bg-rose-100 text-rose-800",
            false,
            0u32,
            &row.capabilities,
        )
    } else if let Some(snap) = live {
        match snap.state {
            ConnState::Active => (
                "online",
                "bg-emerald-100 text-emerald-800",
                true,
                snap.in_flight,
                &snap.capabilities,
            ),
            ConnState::Disconnecting => (
                "draining",
                "bg-amber-100 text-amber-800",
                true,
                snap.in_flight,
                &snap.capabilities,
            ),
        }
    } else {
        (
            "offline",
            "bg-slate-200 text-slate-700",
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
/// trailing path and routes to the right page. Per Q97, the per-job
/// detail endpoint is content-negotiated: `Accept: application/json`
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
    let evals = state.store.list_by_repo(repo.id, 50).await?;

    let snap = state.current.load_full();
    let forge_cfg = snap.config.forges.get(&forge);
    let repo_url = repo_url_for(repo.web_url.as_deref(), forge_cfg, slug.as_str());

    let evals = evals
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
    let html = render(&RepoTemplate {
        cluster_active,
        forge,
        slug: slug.as_str().to_string(),
        name: repo.name,
        description: repo.description,
        repo_url,
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
/// transport accounting (M16) — bytes through our russh tunnel and
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
        // M16 per-phase transport accounting. `null` for jobs that
        // pre-date the column-set or were built locally.
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
/// page's M16 transport rows. Three sigfigs is plenty — "523 MB" is
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
        // the template's empty-state branches all compile + render.
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
