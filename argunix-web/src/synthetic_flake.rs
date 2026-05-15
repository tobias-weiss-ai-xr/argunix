//! Synthetic-flake endpoint.
//!
//! Lets `nix run https://argunix.example.com/flake/<forge>/<slug>/eval/<id>#app`
//! resolve to already-cached store paths without re-evaluating the
//! upstream repo. The handler emits a tar containing one `flake.nix`
//! whose outputs are `builtins.storePath` references plus matching
//! `apps.<system>.<attr>` entries pointing at the cached binary. Nix's
//! tarball flake fetcher accepts any HTTPS URL whose body is a tar with
//! a top-level `flake.nix`; the path/extension don't matter, the
//! response Content-Type does.
//!
//! See `argunix-store/migrations/0013_*` for the per-job persistence
//! that backs the emitted attrs (`main_program`, `outputs_json`).

use crate::state::AppState;
use argunix_config::BinaryCache;
use argunix_domain::{EvalId, Slug};
use argunix_store::{EvalStore, JobRecord, JobStore, RepoStore};
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::collections::BTreeMap;

/// Errors the synthetic-flake endpoint can surface to the client.
///
/// Distinct from `UiError` because the status-code mapping is
/// different (we want 409 for "no public cache configured", not the
/// generic 500), and because the client here is `nix`, not a browser
/// — the bodies are intentionally short plain-text so they show up
/// readably in `nix` error output.
#[derive(Debug, thiserror::Error)]
pub enum SyntheticFlakeError {
    #[error("not found")]
    NotFound,
    #[error(
        "this argunix instance has no public binary cache configured \
         (need a `binary_caches` entry with both `public_url` and \
         `public_key` set); synthetic flakes need a substituter to \
         point users at"
    )]
    NoPublicCache,
    #[error("eval has no successful jobs to expose as a synthetic flake")]
    EmptyEval,
    #[error("store: {0}")]
    Store(#[from] argunix_store::StoreError),
}

impl IntoResponse for SyntheticFlakeError {
    fn into_response(self) -> Response {
        let status = match &self {
            SyntheticFlakeError::NotFound | SyntheticFlakeError::EmptyEval => StatusCode::NOT_FOUND,
            // 409 Conflict reads as "the eval exists but the server's
            // current state conflicts with serving it" — closer to
            // the truth than 404 (eval *is* there) or 503 (not transient).
            SyntheticFlakeError::NoPublicCache => StatusCode::CONFLICT,
            SyntheticFlakeError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %self, "synthetic-flake handler failed");
        }
        (status, self.to_string()).into_response()
    }
}

/// `GET /flake/{forge}/{*tail}` — `tail` is one of:
///
/// - `<slug>/eval/<id>`        immutable, exact eval
/// - `<slug>/ref/<branch>`     mutable, latest green eval on `<branch>`
/// - `<slug>`                  mutable, latest green eval on the repo's
///                             default branch (forge-supplied; 404 if
///                             argunix hasn't seen a webhook for the
///                             repo yet and so doesn't know which
///                             branch counts as default).
///
/// Multi-segment slugs (GitLab subgroups) are tolerated because the
/// `/eval/`, `/ref/` markers uniquely separate slug from selector.
pub async fn serve(
    AxumPath((forge, tail)): AxumPath<(String, String)>,
    State(state): State<AppState>,
) -> Result<Response, SyntheticFlakeError> {
    let parsed = parse_tail(&tail).ok_or(SyntheticFlakeError::NotFound)?;
    let slug = Slug::new(parsed.slug.to_string()).map_err(|_| SyntheticFlakeError::NotFound)?;

    let snapshot = state.current.load_full();
    let caches = usable_public_caches(&snapshot.config.binary_caches);
    if caches.is_empty() {
        return Err(SyntheticFlakeError::NoPublicCache);
    }

    // Multiple traits provide `get` (RepoStore, EvalStore, JobStore) on
    // SqlxStore, so we disambiguate via fully-qualified syntax even
    // though method-call works elsewhere where only one trait is in scope.
    let repo = RepoStore::find(&state.store, &forge, &slug)
        .await?
        .ok_or(SyntheticFlakeError::NotFound)?;

    let (eval, mutable) = match parsed.selector {
        Selector::EvalId(id) => {
            let e = EvalStore::get(&state.store, id)
                .await?
                .filter(|e| e.repo_id == repo.id)
                .ok_or(SyntheticFlakeError::NotFound)?;
            (e, false)
        }
        Selector::LatestOnRef(branch) => {
            let git_ref = normalize_branch_to_git_ref(branch);
            let e = EvalStore::latest_done_for_ref(&state.store, repo.id, &git_ref)
                .await?
                .ok_or(SyntheticFlakeError::NotFound)?;
            (e, true)
        }
        Selector::LatestDefault => {
            // No `default_branch` means no webhook has populated the
            // repo metadata yet — we genuinely don't know which branch
            // to point at. 404 is more honest than guessing `main`.
            let default_branch = repo
                .default_branch
                .as_deref()
                .ok_or(SyntheticFlakeError::NotFound)?;
            let git_ref = normalize_branch_to_git_ref(default_branch);
            let e = EvalStore::latest_done_for_ref(&state.store, repo.id, &git_ref)
                .await?
                .ok_or(SyntheticFlakeError::NotFound)?;
            (e, true)
        }
    };

    let jobs = JobStore::list_by_eval(&state.store, eval.id).await?;
    let entries = jobs
        .into_iter()
        .filter_map(FlakeEntry::from_job)
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Err(SyntheticFlakeError::EmptyEval);
    }

    let flake_src = render_flake_nix(
        &format!("argunix cached: {forge}/{slug} @ {}", eval.sha.short()),
        &caches,
        &entries,
    );
    let body = tar_single_file("flake.nix", flake_src.as_bytes());

    let mut resp = (StatusCode::OK, body).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-tar"),
    );
    // `/eval/<id>` is content-addressed (same sha, same green jobs
    // forever — failed jobs that get retried create new evals, not
    // mutations), so tell nix it can cache forever. The mutable forms
    // (`/ref/<branch>` and bare slug) need shorter TTL because their
    // resolution changes when a new eval lands.
    let cache_control = if mutable {
        "public, max-age=60"
    } else {
        "public, max-age=31536000, immutable"
    };
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    Ok(resp)
}

/// Normalize the URL-supplied branch name to the bare form push-event
/// rows store in `evaluations.git_ref`. The webhook ingest path strips
/// `refs/heads/` on insert (see `argunix-web/src/webhook.rs`), so the
/// `/ref/refs/heads/main` URL must match the same DB rows as `/ref/main`.
fn normalize_branch_to_git_ref(branch: &str) -> String {
    branch
        .strip_prefix("refs/heads/")
        .unwrap_or(branch)
        .to_string()
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedTail<'a> {
    slug: &'a str,
    selector: Selector<'a>,
}

#[derive(Debug, PartialEq, Eq)]
enum Selector<'a> {
    /// `/eval/<id>` — immutable, exact eval row.
    EvalId(EvalId),
    /// `/ref/<branch>` — mutable, latest green eval on that branch.
    LatestOnRef(&'a str),
    /// bare slug — mutable, latest green eval on the repo's default
    /// branch as reported by its forge webhooks.
    LatestDefault,
}

/// Parse the tail of `/flake/<forge>/<tail>` into slug + selector.
/// Markers are checked most-specific-first so a slug that happens to
/// contain `ref` or `eval` doesn't confuse the parser. `rfind` against
/// each marker handles the (unlikely) case where the slug itself
/// contains `/eval/` or `/ref/`.
fn parse_tail(tail: &str) -> Option<ParsedTail<'_>> {
    let tail = tail.trim_end_matches('/');
    if tail.is_empty() {
        return None;
    }
    if let Some(marker) = tail.rfind("/eval/") {
        let slug = &tail[..marker];
        if slug.is_empty() {
            return None;
        }
        let id_str = &tail[marker + "/eval/".len()..];
        if id_str.is_empty() || id_str.contains('/') {
            return None;
        }
        let eval_id: i64 = id_str.parse().ok()?;
        return Some(ParsedTail {
            slug,
            selector: Selector::EvalId(EvalId::new(eval_id)),
        });
    }
    if let Some(marker) = tail.rfind("/ref/") {
        let slug = &tail[..marker];
        if slug.is_empty() {
            return None;
        }
        let branch = &tail[marker + "/ref/".len()..];
        if branch.is_empty() {
            return None;
        }
        return Some(ParsedTail {
            slug,
            selector: Selector::LatestOnRef(branch),
        });
    }
    Some(ParsedTail {
        slug: tail,
        selector: Selector::LatestDefault,
    })
}

/// `BinaryCache` rows that have both a public-facing URL and the
/// matching verifying public key — the minimum needed to point a
/// downstream nix at this cache via flake `nixConfig`.
fn usable_public_caches(all: &[BinaryCache]) -> Vec<UsableCache> {
    all.iter()
        .filter_map(
            |c| match (c.public_url.as_deref(), c.public_key.as_deref()) {
                (Some(url), Some(key)) => Some(UsableCache {
                    url: url.to_string(),
                    key: key.to_string(),
                }),
                _ => None,
            },
        )
        .collect()
}

#[derive(Debug, Clone)]
struct UsableCache {
    url: String,
    key: String,
}

/// One attr that the synthetic flake will expose. Carries everything
/// needed to emit a `packages.<system>.<attr>` entry plus an
/// `apps.<system>.<attr>` entry when `main_program` was captured.
#[derive(Debug, Clone)]
struct FlakeEntry {
    system: String,
    /// Last segment of the source attr_path — what shows up under
    /// `packages.<system>.<here>` in the synthetic flake.
    leaf: String,
    /// Path used as `builtins.storePath` for the package entry.
    /// Always the `out` output (the only one `storePath` can preserve;
    /// see the design discussion above).
    package_out: String,
    /// Store-path output that contains the executable: `outputs["bin"]`
    /// when the derivation has a dedicated `bin` output, otherwise
    /// `outputs["out"]`. Used to render `apps.*.program`.
    bin_output: String,
    /// `meta.mainProgram` — the basename inside `<bin_output>/bin/`.
    /// None means no `apps.*` entry is emitted for this job
    /// (`packages.*` still is, so `nix build` keeps working).
    main_program: Option<String>,
}

impl FlakeEntry {
    fn from_job(job: JobRecord) -> Option<Self> {
        if !job.status.is_success() {
            return None;
        }
        // Only `packages.<system>.<name>` jobs are exposed. devShells,
        // checks, nixosConfigurations and similar flake outputs all
        // also land as eval jobs and they share leaf names with
        // packages (e.g. `devShells.x86_64-linux.default` vs
        // `packages.x86_64-linux.default`) — surfacing them all would
        // make leafs collide and silently shadow each other inside
        // the synthetic flake's `packages.<system>` attrset. We only
        // know how to express *packages* as `builtins.storePath`/
        // `fetchClosure` outputs anyway; the rest don't belong in the
        // synthetic flake even when they happen to have built green.
        let leaf = packages_leaf(job.attr_path.as_str(), &job.system)?.to_string();

        // `out` is the only output we can faithfully expose via
        // `fetchClosure { inputAddressed = true; }` (it can't
        // reconstruct multi-output attrsets). Skip jobs with no
        // `out` — they're rare enough (single-output `lib`-only
        // derivations) that omitting them beats lying about the
        // structure.
        let out = job.outputs.get("out").cloned()?;

        // `getBin` semantics: prefer a dedicated `bin` output for
        // executables, fall back to `out`. Most packages put binaries
        // in `out/bin/`, so this resolves to `out` for the 95% case.
        let bin_output = job
            .outputs
            .get("bin")
            .cloned()
            .unwrap_or_else(|| out.clone());

        Some(FlakeEntry {
            system: job.system,
            leaf,
            package_out: out,
            bin_output,
            main_program: job.main_program,
        })
    }
}

/// Extract `<name>` from `packages.<system>.<name>`. Returns `None` for
/// any attribute path outside the `packages.<system>.` namespace
/// (devShells, checks, nixosConfigurations, homeConfigurations, …) so
/// the synthetic flake exposes only what it can faithfully reconstruct
/// from a cached store path.
fn packages_leaf<'a>(attr_path: &'a str, system: &str) -> Option<&'a str> {
    let prefix = format!("packages.{system}.");
    let rest = attr_path.strip_prefix(&prefix)?;
    // `packages.<system>.<name>` is the supported shape; nested
    // (`packages.<system>.foo.bar`) isn't expressible the same way in
    // the synthetic flake, so skip those — they'd collide on the
    // outermost segment anyway.
    if rest.is_empty() || rest.contains('.') {
        return None;
    }
    Some(rest)
}

/// Render the synthetic `flake.nix` source. The output is one trivial
/// flake — no inputs, no nixpkgs — that exposes `packages.<sys>.<x>`
/// and `apps.<sys>.<x>` for every successfully-cached job, with each
/// store path resolved at evaluation time via `builtins.fetchClosure`.
///
/// We don't use `builtins.storePath` because it's disallowed in pure
/// flake evaluation — flakes are sealed against arbitrary store
/// references for reproducibility. `fetchClosure` with
/// `inputAddressed = true` is the sanctioned escape hatch: it says
/// "fetch *this exact input-addressed path* from the configured
/// substituter, trusting the cache to have signed it." That's exactly
/// our trust model (operator vouches for cache via signing key).
///
/// Requires `experimental-features = fetch-closure` on the consuming
/// nix; we request it via `nixConfig.extra-experimental-features` so
/// `--accept-flake-config` (or pre-trusted substituters) auto-enables.
fn render_flake_nix(description: &str, caches: &[UsableCache], entries: &[FlakeEntry]) -> String {
    let substituters = render_string_list(caches.iter().map(|c| c.url.as_str()));
    let trusted_keys = render_string_list(caches.iter().map(|c| c.key.as_str()));
    // First configured cache wins as the `fromStore` for fetchClosure.
    // The others stay in `extra-substituters` so nix's normal
    // substituter fallback still picks them up for the rest of the
    // closure.
    let fetch_from = caches.first().map(|c| c.url.as_str()).unwrap_or("");

    // Group by system → attr, preserving deterministic order so the
    // emitted flake is byte-identical for the same eval. Nix tarball
    // fetching keys on the narHash; stable output → stable hash →
    // cache hits across requests.
    let mut by_system: BTreeMap<&str, BTreeMap<&str, &FlakeEntry>> = BTreeMap::new();
    for e in entries {
        by_system
            .entry(e.system.as_str())
            .or_default()
            .insert(e.leaf.as_str(), e);
    }

    let mut packages_block = String::new();
    let mut apps_block = String::new();
    for (system, attrs) in &by_system {
        packages_block.push_str(&format!("    packages.{system} = {{\n"));
        for (leaf, e) in attrs {
            packages_block.push_str(&format!(
                "      {leaf} = fetch {path};\n",
                path = e.package_out,
            ));
        }
        packages_block.push_str("    };\n");

        let with_apps: Vec<_> = attrs
            .iter()
            .filter_map(|(leaf, e)| e.main_program.as_deref().map(|mp| (leaf, e, mp)))
            .collect();
        if !with_apps.is_empty() {
            apps_block.push_str(&format!("    apps.{system} = {{\n"));
            for (leaf, e, mp) in with_apps {
                // String-interpolation around `fetch` propagates the
                // store path's context, so `nix run` realises the
                // closure (via the substituter) before exec'ing.
                apps_block.push_str(&format!(
                    "      {leaf} = {{\n\
                     \x20       type = \"app\";\n\
                     \x20       program = \"${{fetch {bin}}}/bin/{mp}\";\n\
                     \x20     }};\n",
                    bin = e.bin_output,
                ));
            }
            apps_block.push_str("    };\n");
        }
    }

    format!(
        r#"{{
  description = "{description}";

  nixConfig = {{
    extra-substituters = {substituters};
    extra-trusted-public-keys = {trusted_keys};
    extra-experimental-features = [ "fetch-closure" ];
  }};

  outputs = _:
    let
      fetch = path: builtins.fetchClosure {{
        fromStore = "{fetch_from}";
        fromPath = path;
        inputAddressed = true;
      }};
    in {{
{packages_block}{apps_block}    }};
}}
"#,
    )
}

fn render_string_list<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::from("[ ");
    for s in items {
        out.push('"');
        out.push_str(s);
        out.push_str("\" ");
    }
    out.push(']');
    out
}

/// Pack one file into a tar archive in memory.
fn tar_single_file(name: &str, contents: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(contents.len() + 1024);
    {
        let mut ar = tar::Builder::new(&mut buf);
        let mut header = tar::Header::new_gnu();
        header
            .set_path(name)
            .expect("static `flake.nix` path encodes");
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        ar.append(&header, contents)
            .expect("writing into a Vec<u8> can't fail");
        ar.finish().expect("writing into a Vec<u8> can't fail");
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use argunix_domain::{AttrPath, EvalId, JobId, JobStatus};

    fn job(attr: &str, system: &str, status: JobStatus, out: Option<&str>) -> JobRecord {
        let mut outputs = BTreeMap::new();
        if let Some(o) = out {
            outputs.insert("out".to_string(), o.to_string());
        }
        JobRecord {
            id: JobId::new(1),
            eval_id: EvalId::new(1),
            attr_path: AttrPath::new(attr),
            drv_path: None,
            system: system.to_string(),
            started_at: None,
            finished_at: None,
            status,
            log_path: None,
            output_path: out.map(str::to_string),
            builder_id: None,
            interrupt_count: 0,
            failure_reason: None,
            phase_metrics: Default::default(),
            main_program: None,
            outputs,
        }
    }

    #[test]
    fn parses_eval_tail() {
        let p = parse_tail("owner/repo/eval/42").unwrap();
        assert_eq!(p.slug, "owner/repo");
        assert_eq!(p.selector, Selector::EvalId(EvalId::new(42)));
    }

    #[test]
    fn parses_subgroup_slug() {
        let p = parse_tail("group/sub/proj/eval/7").unwrap();
        assert_eq!(p.slug, "group/sub/proj");
        assert_eq!(p.selector, Selector::EvalId(EvalId::new(7)));
    }

    #[test]
    fn parses_ref_tail() {
        let p = parse_tail("owner/repo/ref/main").unwrap();
        assert_eq!(p.slug, "owner/repo");
        assert_eq!(p.selector, Selector::LatestOnRef("main"));
    }

    #[test]
    fn parses_ref_with_slashes() {
        // Branch names can contain slashes (`feature/foo`), and so can
        // multi-segment slugs (GitLab subgroups). `rfind` of `/ref/`
        // pins the boundary at the last occurrence — the more common
        // case being a branch name on a non-subgroup slug.
        let p = parse_tail("owner/repo/ref/feature/foo").unwrap();
        assert_eq!(p.slug, "owner/repo");
        assert_eq!(p.selector, Selector::LatestOnRef("feature/foo"));
    }

    #[test]
    fn bare_slug_is_latest_default() {
        // No marker → mutable "latest on default branch".
        let p = parse_tail("owner/repo").unwrap();
        assert_eq!(p.slug, "owner/repo");
        assert_eq!(p.selector, Selector::LatestDefault);
    }

    #[test]
    fn bare_subgroup_slug_is_latest_default() {
        let p = parse_tail("group/sub/proj").unwrap();
        assert_eq!(p.slug, "group/sub/proj");
        assert_eq!(p.selector, Selector::LatestDefault);
    }

    #[test]
    fn empty_tail_rejected() {
        assert!(parse_tail("").is_none());
        assert!(parse_tail("/").is_none());
    }

    #[test]
    fn rejects_trailing_garbage_after_eval_id() {
        // `<id>/something` (e.g. a job sub-path) is unambiguously a
        // malformed eval URL — once `/eval/` is anchored as the
        // marker, the id-suffix must not contain another `/`. We
        // reject hard here rather than falling through to a
        // LatestDefault interpretation, because the user clearly
        // meant `/eval/<id>` and an opaque 404 on a bogus slug is
        // worse than a clear "not found" on the eval URL.
        assert!(parse_tail("owner/repo/eval/42/job/foo").is_none());
    }

    #[test]
    fn normalizes_branch_to_git_ref() {
        // Push events land in `evaluations.git_ref` with the
        // `refs/heads/` prefix already stripped (see webhook.rs).
        // The normalize fn must agree with that on-disk shape, and
        // tolerate `/ref/refs/heads/main` as an alias of `/ref/main`.
        assert_eq!(normalize_branch_to_git_ref("main"), "main");
        assert_eq!(normalize_branch_to_git_ref("refs/heads/main"), "main");
        // Slash-bearing branch (`feature/foo`).
        assert_eq!(normalize_branch_to_git_ref("feature/foo"), "feature/foo",);
        assert_eq!(
            normalize_branch_to_git_ref("refs/heads/feature/foo"),
            "feature/foo",
        );
    }

    #[test]
    fn skips_failed_and_outputless_jobs() {
        // A job that built fine but happens to have no `out` (rare —
        // single-output `lib` derivation) is skipped with a brief
        // explanation in [`FlakeEntry::from_job`].
        assert!(
            FlakeEntry::from_job(job(
                "packages.x86_64-linux.foo",
                "x86_64-linux",
                JobStatus::Failure,
                Some("/nix/store/x-foo"),
            ))
            .is_none(),
            "failed jobs must not be exposed",
        );
        assert!(
            FlakeEntry::from_job(job(
                "packages.x86_64-linux.bar",
                "x86_64-linux",
                JobStatus::Success,
                None,
            ))
            .is_none(),
            "jobs with no `out` output must not be exposed",
        );
    }

    #[test]
    fn from_job_uses_bin_output_when_present() {
        let mut j = job(
            "packages.x86_64-linux.hello",
            "x86_64-linux",
            JobStatus::Success,
            Some("/nix/store/zzz-hello"),
        );
        j.outputs
            .insert("bin".to_string(), "/nix/store/yyy-hello-bin".to_string());
        j.main_program = Some("hello".to_string());
        let entry = FlakeEntry::from_job(j).expect("entry");
        // package always points at `out`...
        assert_eq!(entry.package_out, "/nix/store/zzz-hello");
        // ...but the executable lookup follows getBin semantics, so
        // it picks the dedicated bin output.
        assert_eq!(entry.bin_output, "/nix/store/yyy-hello-bin");
        assert_eq!(entry.main_program.as_deref(), Some("hello"));
    }

    #[test]
    fn from_job_falls_back_to_out_for_apps() {
        let mut j = job(
            "packages.x86_64-linux.hello",
            "x86_64-linux",
            JobStatus::Success,
            Some("/nix/store/zzz-hello"),
        );
        j.main_program = Some("hello".to_string());
        let entry = FlakeEntry::from_job(j).expect("entry");
        // No `bin` output → bin_output mirrors `out`.
        assert_eq!(entry.bin_output, "/nix/store/zzz-hello");
    }

    #[test]
    fn from_job_omits_apps_without_main_program() {
        // `meta.mainProgram` was missing → we still emit the package
        // entry (so `nix build` works) but skip the app entry.
        let entry = FlakeEntry::from_job(job(
            "packages.x86_64-linux.bar",
            "x86_64-linux",
            JobStatus::Success,
            Some("/nix/store/q-bar"),
        ))
        .expect("entry");
        assert!(entry.main_program.is_none());
    }

    #[test]
    fn from_job_filters_non_packages_attrs() {
        // Anything outside `packages.<system>.<name>` is dropped — a
        // green `devShells.x86_64-linux.default` job would otherwise
        // collide with `packages.x86_64-linux.default` and silently
        // shadow it inside the rendered flake. Same story for
        // `checks`, `nixosConfigurations`, etc.
        for attr in [
            "devShells.x86_64-linux.default",
            "checks.x86_64-linux.smoke",
            "nixosConfigurations.demo",
            "homeConfigurations.user",
            // Nested `packages.<system>.foo.bar` isn't representable
            // as a single `builtins.fetchClosure` entry either —
            // surface that as "skipped" too.
            "packages.x86_64-linux.suite.nested",
        ] {
            assert!(
                FlakeEntry::from_job(job(
                    attr,
                    "x86_64-linux",
                    JobStatus::Success,
                    Some("/nix/store/x-thing"),
                ))
                .is_none(),
                "`{attr}` must be filtered out",
            );
        }
    }

    #[test]
    fn from_job_keeps_packages_default() {
        // The whole point of the filter: when both
        // `packages.<sys>.default` and `devShells.<sys>.default` exist,
        // we keep the packages one (and the devShells one drops out
        // entirely via the test above).
        let entry = FlakeEntry::from_job(job(
            "packages.x86_64-linux.default",
            "x86_64-linux",
            JobStatus::Success,
            Some("/nix/store/q-default"),
        ))
        .expect("entry");
        assert_eq!(entry.leaf, "default");
    }

    #[test]
    fn packages_leaf_pins_to_matching_system() {
        // The system in the attr path must match the job's `system`
        // column — a cross-system row (shouldn't happen, but defensive)
        // doesn't accidentally promote into the wrong systems block.
        assert_eq!(
            packages_leaf("packages.x86_64-linux.hello", "x86_64-linux"),
            Some("hello"),
        );
        assert_eq!(
            packages_leaf("packages.aarch64-linux.hello", "x86_64-linux"),
            None,
        );
    }

    #[test]
    fn renders_flake_with_packages_and_apps() {
        let caches = vec![UsableCache {
            url: "https://cache.example.com".into(),
            key: "argunix-1:abc".into(),
        }];
        let entries = vec![
            FlakeEntry {
                system: "x86_64-linux".into(),
                leaf: "hello".into(),
                package_out: "/nix/store/zzz-hello".into(),
                bin_output: "/nix/store/zzz-hello".into(),
                main_program: Some("hello".into()),
            },
            FlakeEntry {
                system: "x86_64-linux".into(),
                leaf: "lib-only".into(),
                package_out: "/nix/store/qq-lib".into(),
                bin_output: "/nix/store/qq-lib".into(),
                main_program: None,
            },
        ];
        let src = render_flake_nix("test", &caches, &entries);
        // Substituter + key wired into nixConfig.
        assert!(
            src.contains("https://cache.example.com"),
            "substituter must appear in nixConfig: {src}",
        );
        assert!(
            src.contains("argunix-1:abc"),
            "trusted key must appear in nixConfig: {src}",
        );
        // The flake declares its own `fetch` helper that wraps
        // `builtins.fetchClosure { inputAddressed = true; }`, and
        // requests the `fetch-closure` experimental feature so
        // downstream nix doesn't reject the call.
        assert!(
            src.contains("builtins.fetchClosure"),
            "synthetic flake must use builtins.fetchClosure: {src}",
        );
        assert!(
            src.contains("inputAddressed = true"),
            "fetchClosure call must set inputAddressed = true: {src}",
        );
        assert!(
            src.contains(r#"extra-experimental-features = [ "fetch-closure" ]"#),
            "synthetic flake must enable the fetch-closure feature: {src}",
        );
        // `packages.<system>` block names both attrs.
        assert!(
            src.contains("packages.x86_64-linux"),
            "per-system packages block missing: {src}",
        );
        assert!(
            src.contains("hello = fetch /nix/store/zzz-hello"),
            "package fetch entry missing: {src}",
        );
        assert!(
            src.contains("lib-only = fetch /nix/store/qq-lib"),
            "outputless-app package entry missing: {src}",
        );
        // `apps.<system>` block only contains entries with main_program,
        // and the program path is interpolated through the `fetch`
        // helper so store context propagates.
        assert!(
            src.contains("apps.x86_64-linux"),
            "apps block missing: {src}",
        );
        assert!(
            src.contains(r#"program = "${fetch /nix/store/zzz-hello}/bin/hello""#),
            "app program must interpolate via the fetch helper: {src}",
        );
        // Defence in depth: a context-free literal would also contain
        // the path but not the interpolation.
        assert!(
            !src.contains("program = \"/nix/store/"),
            "app program must not be a context-free literal: {src}",
        );
        assert!(
            !src.contains("lib-only = {"),
            "apps block must NOT include attrs without a main_program: {src}",
        );
    }

    #[test]
    fn render_is_deterministic() {
        // Critical for nix's tarball cache: same eval → same bytes →
        // same narHash → cache hit. Build twice from intentionally
        // shuffled input and compare.
        let caches = vec![UsableCache {
            url: "u".into(),
            key: "k".into(),
        }];
        let a = vec![
            FlakeEntry {
                system: "x86_64-linux".into(),
                leaf: "a".into(),
                package_out: "/p/a".into(),
                bin_output: "/p/a".into(),
                main_program: None,
            },
            FlakeEntry {
                system: "aarch64-linux".into(),
                leaf: "z".into(),
                package_out: "/p/z".into(),
                bin_output: "/p/z".into(),
                main_program: None,
            },
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(
            render_flake_nix("d", &caches, &a),
            render_flake_nix("d", &caches, &b),
        );
    }

    #[test]
    fn tar_round_trips() {
        // Sanity-check that the produced bytes are a valid tar with
        // exactly one `flake.nix` member of the right size — without
        // this, a malformed tar would surface only when nix tried to
        // unpack on the user's machine.
        let body = b"{ outputs = _: {}; }\n";
        let bytes = tar_single_file("flake.nix", body);
        let mut ar = tar::Archive::new(&bytes[..]);
        let mut entries: Vec<_> = ar
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.path().unwrap().to_string_lossy().into_owned(), e.size())
            })
            .collect();
        assert_eq!(entries.len(), 1);
        let (name, size) = entries.pop().unwrap();
        assert_eq!(name, "flake.nix");
        assert_eq!(size, body.len() as u64);
    }

    // Note: `usable_public_caches` isn't unit-tested here because
    // constructing a `BinaryCache` from outside `argunix-config`
    // requires a `SecretFile` — which has no public constructor —
    // and adding one just for this test feels worse than the small
    // risk this filter is wrong. The branch is two-and-a-half lines
    // and covered end-to-end by the live nix-run smoke test.
}
