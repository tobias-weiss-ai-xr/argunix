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
use medusa_domain::{EvalId, EvalStatus, JobStatus, Slug};
use medusa_store::{EvalStore, JobStore, RepoStore};

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
