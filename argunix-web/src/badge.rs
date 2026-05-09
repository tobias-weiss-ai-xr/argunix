//! Status-badge SVG endpoint.
//!
//! Two routes:
//!   GET /badge/<forge>/<...slug>.svg            — latest eval (any branch)
//!   GET /badge/<forge>/<...slug>/<branch>.svg   — latest eval matching `branch`
//!
//! Returned SVG is a small two-pill design: left chip says "argunix",
//! right chip carries the status label. Colour is keyed on the
//! `EvalStatus` of the most recent terminal eval. In-flight evals are
//! shown as `pending` so a freshly-pushed badge doesn't go to red until
//! a real failure lands.
//!
//! The endpoint is self-contained on purpose — no askama templates,
//! no shields.io dependency — to keep the surface tiny and the SVG
//! rendering deterministic for tests.

use crate::state::AppState;
use crate::ui::{UiError, repo_url_for};
use argunix_domain::{EvalStatus, Slug};
use argunix_store::{EvalStore, RepoStore};
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Status the badge surfaces. Distinct from `EvalStatus` so we can
/// collapse in-flight states (queued/evaluating/building) into one
/// `Pending` bucket and missing-data into `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BadgeStatus {
    Passing,
    Failing,
    Cancelled,
    Pending,
    Unknown,
}

impl BadgeStatus {
    fn label(self) -> &'static str {
        match self {
            BadgeStatus::Passing => "passing",
            BadgeStatus::Failing => "failing",
            BadgeStatus::Cancelled => "cancelled",
            BadgeStatus::Pending => "pending",
            BadgeStatus::Unknown => "unknown",
        }
    }

    /// Right-chip background colour. Picked from the same green/red/grey
    /// palette as shields.io so the badges look at home next to other
    /// CI badges in a README.
    fn colour(self) -> &'static str {
        match self {
            BadgeStatus::Passing => "#4c1",
            BadgeStatus::Failing => "#e05d44",
            BadgeStatus::Cancelled => "#9f9f9f",
            BadgeStatus::Pending => "#dfb317",
            BadgeStatus::Unknown => "#9f9f9f",
        }
    }
}

fn classify(status: &EvalStatus) -> BadgeStatus {
    match status {
        EvalStatus::Done => BadgeStatus::Passing,
        EvalStatus::EvaluationFailed => BadgeStatus::Failing,
        EvalStatus::Cancelled => BadgeStatus::Cancelled,
        EvalStatus::Queued | EvalStatus::Evaluating | EvalStatus::Building => BadgeStatus::Pending,
    }
}

/// `GET /badge/<forge>/<...tail>` where the tail is either
/// `<slug>.svg` or `<slug>/<branch>.svg`. We dispatch on the suffix —
/// the slug itself can contain slashes (gitlab subgroups), but the
/// branch segment never does in practice for hosted forges. To
/// disambiguate, we look for the trailing `.svg`, strip it, then split
/// off the branch as the last `/`-segment iff the *remainder before
/// the last `/`* points at a known repo.
pub async fn handle(
    AxumPath((forge, tail)): AxumPath<(String, String)>,
    State(state): State<AppState>,
) -> Result<Response, UiError> {
    let (slug_str, branch) = parse_badge_tail(&tail).ok_or(UiError::NotFound)?;

    // Try the unambiguous "no branch" form first: the entire path
    // before `.svg` is the slug. If we find a repo for it, render the
    // any-branch badge. Otherwise, treat the last `/`-segment as a
    // branch and the remainder as the slug.
    let (repo, branch_filter) = match resolve_repo(&state, &forge, slug_str).await? {
        Some(r) => (r, branch),
        None => {
            // Pull the trailing segment off and retry with everything
            // before as the slug. If that still fails, 404.
            let (parent_slug, last) = slug_str.rsplit_once('/').ok_or(UiError::NotFound)?;
            let repo = resolve_repo(&state, &forge, parent_slug)
                .await?
                .ok_or(UiError::NotFound)?;
            (repo, Some(last))
        }
    };

    let evals = state.store.list_by_repo(repo.id, 50).await?;
    let chosen = pick_eval(&evals, branch_filter);
    let status = chosen
        .map(|e| classify(&e.status))
        .unwrap_or(BadgeStatus::Unknown);

    let svg = render_svg(status);
    let mut resp = (StatusCode::OK, svg).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml; charset=utf-8"),
    );
    // Short cache so README badges don't beat up the daemon, but stay
    // responsive enough that a successful build flips the badge within
    // a minute or two for casual viewers.
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    Ok(resp)
}

/// Strip the `.svg` suffix and return `(path_before_dot_svg, None)`
/// — the caller decides whether to interpret the trailing segment as
/// a branch by attempting repo lookups. Returns None if `.svg` is
/// missing or the path is empty.
fn parse_badge_tail(tail: &str) -> Option<(&str, Option<&str>)> {
    let stripped = tail.strip_suffix(".svg")?;
    if stripped.is_empty() {
        return None;
    }
    Some((stripped, None))
}

async fn resolve_repo(
    state: &AppState,
    forge: &str,
    slug_str: &str,
) -> Result<Option<argunix_store::RepoRecord>, UiError> {
    let slug = match Slug::new(slug_str.to_string()) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    Ok(state.store.find(forge, &slug).await?)
}

/// Pick the eval to badge from a list of recent rows (newest first).
/// `branch_filter` filters by the trailing component of `git_ref` —
/// PR refs (`refs/pull/N/head:branch`) match on the post-`:` branch,
/// push refs match by exact ref. Returns the first terminal eval that
/// matches, falling back to the first matching in-flight eval, then
/// `None` for "no data".
fn pick_eval<'a>(
    evals: &'a [argunix_store::EvalRecord],
    branch_filter: Option<&str>,
) -> Option<&'a argunix_store::EvalRecord> {
    let matches = |e: &argunix_store::EvalRecord| -> bool {
        let Some(branch) = branch_filter else {
            return true;
        };
        let tail = e
            .git_ref
            .rsplit_once(':')
            .map_or(e.git_ref.as_str(), |(_, b)| b);
        tail == branch
    };

    // Prefer terminal evals (the user wants the latest *result*, not a
    // mid-flight pending state) — fall back to in-flight only when no
    // terminal eval matches the filter yet.
    evals
        .iter()
        .find(|e| matches(e) && e.status.is_terminal())
        .or_else(|| evals.iter().find(|e| matches(e)))
}

/// Render a two-pill SVG. Widths are derived from a fixed
/// glyph-width estimate (6.5px/char @ 11px font) so the badge sizes
/// itself sensibly without a font-metrics library.
fn render_svg(status: BadgeStatus) -> String {
    const LABEL: &str = "argunix";
    let message = status.label();
    let pad = 12u32;
    let glyph = 6u32; // average glyph advance at 11px Verdana — close enough
    let label_w = pad * 2 + (LABEL.len() as u32) * glyph;
    let msg_w = pad * 2 + (message.len() as u32) * glyph;
    let total_w = label_w + msg_w;
    let h = 20u32;
    let label_x = label_w / 2;
    let msg_x = label_w + msg_w / 2;
    let colour = status.colour();

    // Hand-rolled SVG matches shields.io's "flat" style closely enough
    // to not look out of place. Linear gradient gives the subtle top
    // highlight; rounded corners keep the rectangle from looking harsh
    // at small sizes. `aria-label` is the accessible single-line
    // summary screen readers will announce.
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{total_w}" height="{h}" role="img" aria-label="argunix: {message}">
<title>argunix: {message}</title>
<linearGradient id="s" x2="0" y2="100%">
<stop offset="0" stop-color="#bbb" stop-opacity=".1"/>
<stop offset="1" stop-opacity=".1"/>
</linearGradient>
<clipPath id="r"><rect width="{total_w}" height="{h}" rx="3" fill="#fff"/></clipPath>
<g clip-path="url(#r)">
<rect width="{label_w}" height="{h}" fill="#555"/>
<rect x="{label_w}" width="{msg_w}" height="{h}" fill="{colour}"/>
<rect width="{total_w}" height="{h}" fill="url(#s)"/>
</g>
<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11">
<text x="{label_x}" y="14">{LABEL}</text>
<text x="{msg_x}" y="14">{message}</text>
</g>
</svg>"##
    )
}

/// Thin wrapper used by the repo page to render the markdown snippet
/// users copy into their READMEs. Public to the crate so the UI module
/// doesn't have to know the URL shape.
pub fn markdown_snippet(host: &str, forge: &str, slug: &str, repo_url: &str) -> String {
    // Trailing slash on `host` would produce `host//badge/...` — guard
    // against that so the snippet is paste-clean.
    let host = host.trim_end_matches('/');
    format!(
        "[![argunix]({host}/badge/{forge}/{slug}.svg)]({repo_url})",
        host = host,
        forge = forge,
        slug = slug,
        repo_url = repo_url,
    )
}

/// Build the badge URL itself — used both by the markdown snippet and
/// by the repo page's "raw URL" copy field.
pub fn badge_url(host: &str, forge: &str, slug: &str) -> String {
    let host = host.trim_end_matches('/');
    format!("{host}/badge/{forge}/{slug}.svg")
}

// repo_url_for stays in ui.rs; we just import it here for snippet
// callers that already pass the resolved URL in directly.
#[allow(dead_code)]
fn _ensure_repo_url_visible(
    url: Option<&str>,
    cfg: Option<&argunix_config::ForgeConfig>,
    slug: &str,
) -> String {
    repo_url_for(url, cfg, slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_badge_tail_strips_svg() {
        assert_eq!(
            parse_badge_tail("owner/repo.svg"),
            Some(("owner/repo", None))
        );
        assert_eq!(parse_badge_tail("a/b/c.svg"), Some(("a/b/c", None)));
    }

    #[test]
    fn parses_badge_tail_rejects_no_suffix() {
        assert_eq!(parse_badge_tail("owner/repo"), None);
        assert_eq!(parse_badge_tail(".svg"), None);
        assert_eq!(parse_badge_tail(""), None);
    }

    #[test]
    fn classify_maps_terminal_states() {
        assert_eq!(classify(&EvalStatus::Done), BadgeStatus::Passing);
        assert_eq!(
            classify(&EvalStatus::EvaluationFailed),
            BadgeStatus::Failing
        );
        assert_eq!(classify(&EvalStatus::Cancelled), BadgeStatus::Cancelled);
    }

    #[test]
    fn classify_collapses_in_flight_to_pending() {
        assert_eq!(classify(&EvalStatus::Queued), BadgeStatus::Pending);
        assert_eq!(classify(&EvalStatus::Evaluating), BadgeStatus::Pending);
        assert_eq!(classify(&EvalStatus::Building), BadgeStatus::Pending);
    }

    fn record(id: i64, status: EvalStatus, git_ref: &str) -> argunix_store::EvalRecord {
        use argunix_domain::{EvalId, RepoId, Sha};
        argunix_store::EvalRecord {
            id: EvalId::new(id),
            repo_id: RepoId::new(1),
            trigger: "push".into(),
            git_ref: git_ref.into(),
            sha: Sha::new("a".repeat(40)).unwrap(),
            started_at: None,
            finished_at: None,
            status,
            pr_number: None,
            building_started_at: None,
            failure_reason: None,
        }
    }

    #[test]
    fn pick_eval_prefers_terminal_over_in_flight() {
        // Newest-first input — a fresh `Building` row should not mask
        // the previous green/red status the user actually wants to see.
        let evals = vec![
            record(3, EvalStatus::Building, "main"),
            record(2, EvalStatus::Done, "main"),
            record(1, EvalStatus::EvaluationFailed, "main"),
        ];
        let picked = pick_eval(&evals, None).unwrap();
        assert_eq!(picked.id.get(), 2);
    }

    #[test]
    fn pick_eval_falls_back_to_in_flight_when_no_terminal_match() {
        let evals = vec![record(1, EvalStatus::Building, "main")];
        let picked = pick_eval(&evals, None).unwrap();
        assert_eq!(picked.id.get(), 1);
    }

    #[test]
    fn pick_eval_filters_by_branch_via_pr_ref_tail() {
        let evals = vec![
            record(3, EvalStatus::Done, "main"),
            record(2, EvalStatus::Done, "refs/pull/7/head:feature-x"),
            record(1, EvalStatus::EvaluationFailed, "main"),
        ];
        let picked = pick_eval(&evals, Some("feature-x")).unwrap();
        assert_eq!(picked.id.get(), 2);
    }

    #[test]
    fn pick_eval_returns_none_when_no_match() {
        let evals = vec![record(1, EvalStatus::Done, "main")];
        assert!(pick_eval(&evals, Some("missing")).is_none());
    }

    #[test]
    fn render_svg_includes_label_and_message() {
        let svg = render_svg(BadgeStatus::Passing);
        assert!(svg.contains("argunix"));
        assert!(svg.contains("passing"));
        // Right pill colour in the green family.
        assert!(svg.contains("#4c1"));
        // Width attribute is present and non-zero.
        assert!(svg.contains(r#"width=""#));
        assert!(!svg.contains(r#"width="0""#));
    }

    #[test]
    fn markdown_snippet_uses_badge_url_and_repo_url() {
        let snippet = markdown_snippet(
            "https://argunix.example.com/",
            "github",
            "owner/repo",
            "https://github.com/owner/repo",
        );
        assert_eq!(
            snippet,
            "[![argunix](https://argunix.example.com/badge/github/owner/repo.svg)](https://github.com/owner/repo)"
        );
    }

    #[test]
    fn badge_url_strips_trailing_slash_on_host() {
        assert_eq!(
            badge_url("https://argunix.example.com/", "github", "owner/repo"),
            "https://argunix.example.com/badge/github/owner/repo.svg"
        );
    }
}
