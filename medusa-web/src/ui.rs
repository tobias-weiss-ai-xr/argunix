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
//! Markup lives in `medusa-web/templates/*.html` and is rendered with
//! Askama (compile-time, type-checked). Static assets — including the
//! Tailwind-compiled `ui.css` referenced by `base.html` — are served
//! separately by a `ServeDir` mounted at `/static` (see `lib.rs`).

use crate::state::AppState;
use askama::Template;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use medusa_builders::ConnState;
use medusa_domain::{EvalId, EvalStatus, JobStatus, Slug};
use medusa_store::{BuilderStore, EvalStore, JobStore, RepoStore};
use std::collections::HashMap;

#[derive(Template)]
#[template(path = "status.html")]
struct StatusTemplate {
    totals: ClusterTotals,
    builders: Vec<BuilderRow>,
    running: Vec<RunningRow>,
    queued: Vec<QueuedRow>,
    queued_shown: usize,
    queued_truncated: bool,
}

struct ClusterTotals {
    builders_online: usize,
    builders_known: usize,
    in_flight: u32,
    total_slots: u32,
    utilization_pct: u32,
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
    systems: String,
    features: String,
    nix_version: String,
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
    repos: Vec<RepoRow>,
}

struct RepoRow {
    forge: String,
    slug: String,
}

#[derive(Template)]
#[template(path = "repo.html")]
struct RepoTemplate {
    forge: String,
    slug: String,
    evals: Vec<EvalRow>,
}

struct EvalRow {
    id: i64,
    git_ref: String,
    short_sha: String,
    status: &'static str,
    finished: String,
}

#[derive(Template)]
#[template(path = "eval.html")]
struct EvalTemplate {
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
    job_heading: String,
    empty_jobs_msg: &'static str,
    jobs: Vec<JobRow>,
}

struct JobRow {
    attr_path: String,
    system: String,
    status: &'static str,
    finished: String,
    has_log: bool,
}

#[derive(Template)]
#[template(path = "job.html")]
struct JobTemplate {
    forge: String,
    slug: String,
    eval_id: i64,
    attr_path: String,
    system: String,
    status_label: &'static str,
    started: String,
    finished: String,
    drv_path: Option<String>,
    output_path: Option<String>,
    has_log: bool,
}

pub async fn index(State(state): State<AppState>) -> Result<Html<String>, UiError> {
    let repos = state
        .store
        .list()
        .await?
        .into_iter()
        .map(|r| RepoRow {
            forge: r.forge,
            slug: r.slug.as_str().to_string(),
        })
        .collect();
    Ok(Html(render(&IndexTemplate { repos })?))
}

/// Cluster status overview — at-a-glance view of every known builder
/// and what the cluster is doing right now. Auto-refreshes via meta tag
/// (see `templates/status.html`).
pub async fn status(State(state): State<AppState>) -> Result<Html<String>, UiError> {
    // Persistent roster: every builder ever enrolled, including offline
    // / revoked ones. Live registry is the runtime overlay.
    let roster = <medusa_store::SqlxStore as BuilderStore>::list_all(&state.store).await?;
    let live = state.builder_registry.list();
    let live_by_name: HashMap<String, &medusa_builders::BuilderSnapshot> = live
        .iter()
        .map(|s| (s.name.as_str().to_string(), s))
        .collect();

    let now = chrono::Utc::now();
    let builders: Vec<BuilderRow> = roster
        .iter()
        .map(|row| build_builder_row(row, live_by_name.get(row.name.as_str()).copied(), now))
        .collect();

    let builders_online = live.iter().filter(|b| b.state == ConnState::Active).count();
    let in_flight: u32 = live.iter().map(|b| b.in_flight).sum();
    let total_slots: u32 = live
        .iter()
        .filter(|b| b.state == ConnState::Active)
        .map(|b| b.capabilities.max_jobs)
        .sum();
    let utilization_pct = if total_slots == 0 {
        0
    } else {
        ((in_flight as u64 * 100) / total_slots as u64) as u32
    };

    // BuilderId → display name map so running rows can show the operator
    // name rather than the opaque numeric id stored on the job row.
    let builder_id_to_name: HashMap<i64, String> = roster
        .iter()
        .map(|r| (r.id.get(), r.name.as_str().to_string()))
        .collect();

    let running_jobs = <medusa_store::SqlxStore as JobStore>::list_running(&state.store).await?;
    let running: Vec<RunningRow> = running_jobs
        .into_iter()
        .map(|j| RunningRow {
            forge: j.forge,
            slug: j.slug.as_str().to_string(),
            eval_id: j.job.eval_id.get(),
            attr_path: j.job.attr_path.to_string(),
            system: j.job.system,
            git_ref: j.git_ref,
            short_sha: j.short_sha,
            builder: j
                .job
                .builder_id
                .and_then(|id| builder_id_to_name.get(&id.get()).cloned())
                .unwrap_or_else(|| "—".to_string()),
            started: fmt_opt_time(j.job.started_at),
        })
        .collect();

    // Pull `LIMIT + 1` so we can tell whether the queue extends past the
    // display cap without an extra COUNT round-trip.
    let queued_jobs =
        <medusa_store::SqlxStore as JobStore>::list_queued(&state.store, QUEUED_DISPLAY_LIMIT + 1)
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

    let totals = ClusterTotals {
        builders_online,
        builders_known: roster.len(),
        in_flight,
        total_slots,
        utilization_pct,
        running: running.len(),
        // queued_jobs was capped at LIMIT+1, so we report ≥ truthfully.
        // For >LIMIT queues we show "LIMIT+" using the truncated flag.
        queued_total,
    };

    Ok(Html(render(&StatusTemplate {
        totals,
        builders,
        running,
        queued,
        queued_shown,
        queued_truncated,
    })?))
}

fn build_builder_row(
    row: &medusa_store::BuilderRecord,
    live: Option<&medusa_builders::BuilderSnapshot>,
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
        systems: caps.systems.join(", "),
        features: if caps.features.is_empty() {
            "—".to_string()
        } else {
            caps.features.join(", ")
        },
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
/// trailing path and routes to the right page.
pub async fn dispatch_repo(
    AxumPath((forge, tail)): AxumPath<(String, String)>,
    state: State<AppState>,
) -> Result<Response, UiError> {
    let parsed = parse_repo_tail(&tail).ok_or(UiError::NotFound)?;
    let slug = Slug::new(parsed.slug.to_string()).map_err(|_| UiError::NotFound)?;

    match parsed.kind {
        TailKind::Repo => repo_page(state, forge, slug).await,
        TailKind::Eval(id) => eval_page(state, forge, slug, EvalId::new(id)).await,
        TailKind::Job { eval_id, attr } => {
            job_page(state, forge, slug, EvalId::new(eval_id), attr.to_string()).await
        }
        TailKind::Log { eval_id, attr } => {
            log_handler(state, forge, slug, EvalId::new(eval_id), attr.to_string()).await
        }
    }
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

    let evals = evals
        .into_iter()
        .map(|e| EvalRow {
            id: e.id.get(),
            git_ref: e.git_ref,
            short_sha: short_sha(e.sha.as_str()).to_string(),
            status: eval_status_label(&e.status),
            finished: fmt_opt_time(e.finished_at),
        })
        .collect();

    let html = render(&RepoTemplate {
        forge,
        slug: slug.as_str().to_string(),
        evals,
    })?;
    Ok(Html(html).into_response())
}

async fn eval_page(
    State(state): State<AppState>,
    forge: String,
    slug: Slug,
    eval_id: EvalId,
) -> Result<Response, UiError> {
    let eval = <medusa_store::SqlxStore as EvalStore>::get(&state.store, eval_id)
        .await?
        .ok_or(UiError::NotFound)?;
    let jobs = <medusa_store::SqlxStore as JobStore>::list_by_eval(&state.store, eval_id).await?;

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
        })
        .collect();

    let html = render(&EvalTemplate {
        forge,
        slug: slug.as_str().to_string(),
        eval_id: eval_id.get(),
        status_label: eval_status_label(&eval.status),
        phase_class: eval_status_phase_class(&eval.status),
        trigger: eval.trigger.to_string(),
        git_ref: eval.git_ref,
        sha: eval.sha.to_string(),
        started: fmt_opt_time(eval.started_at),
        finished: fmt_opt_time(eval.finished_at),
        job_heading,
        empty_jobs_msg,
        jobs: job_rows,
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
    let jobs = <medusa_store::SqlxStore as JobStore>::list_by_eval(&state.store, eval_id).await?;
    let job = jobs
        .into_iter()
        .find(|j| j.attr_path.as_str() == attr)
        .ok_or(UiError::NotFound)?;

    let html = render(&JobTemplate {
        forge,
        slug: slug.as_str().to_string(),
        eval_id: eval_id.get(),
        attr_path: job.attr_path.to_string(),
        system: job.system,
        status_label: job_status_label(&job.status),
        started: fmt_opt_time(job.started_at),
        finished: fmt_opt_time(job.finished_at),
        drv_path: job.drv_path,
        output_path: job.output_path,
        has_log: job.log_path.is_some(),
    })?;
    Ok(Html(html).into_response())
}

/// Stream the zstd-compressed build log decompressed as `text/plain`.
async fn log_handler(
    State(state): State<AppState>,
    _forge: String,
    _slug: Slug,
    eval_id: EvalId,
    attr: String,
) -> Result<Response, UiError> {
    let jobs = <medusa_store::SqlxStore as JobStore>::list_by_eval(&state.store, eval_id).await?;
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
    Store(#[from] medusa_store::StoreError),
    #[error("reading build log: {0}")]
    LogRead(#[source] std::io::Error),
    #[error("rendering template: {0}")]
    Render(#[source] askama::Error),
}

impl IntoResponse for UiError {
    fn into_response(self) -> Response {
        let status = match &self {
            UiError::NotFound => StatusCode::NOT_FOUND,
            UiError::Store(_) | UiError::LogRead(_) | UiError::Render(_) => {
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
            totals: ClusterTotals {
                builders_online: 0,
                builders_known: 0,
                in_flight: 0,
                total_slots: 0,
                utilization_pct: 0,
                running: 0,
                queued_total: 0,
            },
            builders: vec![],
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
