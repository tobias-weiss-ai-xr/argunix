//! Post-build effect orchestration.
//!
//! The generic half of the effects design: turn config into [`Effect`]
//! objects, run them after a successful build, and record one
//! `effect_runs` row per attempt so "did this push happen, when, why
//! did it fail" is answerable from the database. The specialised half
//! — what each effect actually *does* — lives in `argunix-effects`.
//!
//! Both build paths funnel through here: the daemon's worker
//! (`worker.rs`) and the single-shot `argunix build` CLI (`main.rs`).
//!
//! Effects are best-effort. A failing effect is logged and recorded;
//! it never flips the job's terminal status. That matches today's
//! `Severity::Advisory` for both effect kinds — when a `Reported`
//! effect (deploy) arrives, this is the function that grows a status
//! surface for it.

use argunix_domain::JobId;
use argunix_effects::{Effect, EffectStatus, OutputContext, Severity};
use argunix_store::{EffectRunStore, JobStore, SbomStore, SqlxStore};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;

/// Post-build bookkeeping for a successful OCI-image job: stamp the
/// built archive's on-disk size onto the job row, and generate +
/// persist its CycloneDX SBOM. Both are best-effort — a failure is
/// logged and the job stays green. Runs independently of any
/// registry/cache config, so size and SBOM are recorded even for an
/// image that is never pushed.
pub async fn record_image_artifacts(
    store: &SqlxStore,
    job_id: JobId,
    attr_path: &str,
    output_paths: &[String],
    sbom_runtime_roots: &[String],
) {
    if let Some(archive) = output_paths.first() {
        match tokio::fs::metadata(archive).await {
            Ok(meta) => {
                if let Err(e) =
                    <SqlxStore as JobStore>::record_image_size(store, job_id, meta.len()).await
                {
                    tracing::warn!(job_id = job_id.get(), error = %e, "recording image size failed");
                }
            }
            Err(e) => tracing::warn!(
                job_id = job_id.get(),
                path = %archive,
                error = %e,
                "stat of image archive failed; size not recorded",
            ),
        }
    }

    match argunix_effects::sbom::generate_sbom(attr_path, output_paths, sbom_runtime_roots).await {
        Ok((bytes, count)) => {
            let content = String::from_utf8_lossy(&bytes).into_owned();
            if let Err(e) = <SqlxStore as SbomStore>::upsert_sbom(
                store,
                job_id,
                "cyclonedx",
                &content,
                count as u32,
                Utc::now(),
            )
            .await
            {
                tracing::warn!(job_id = job_id.get(), error = %e, "storing SBOM failed");
            } else {
                tracing::info!(job_id = job_id.get(), components = count, "stored SBOM");
            }
        }
        Err(e) => tracing::warn!(
            job_id = job_id.get(),
            error = %e,
            "SBOM generation failed; nothing stored",
        ),
    }
}

/// Build the post-build effects that apply to `repo`, resolving each
/// `push_to_registries` name against the global `registries` catalog.
///
/// Each bound registry yields two effects: a `registry-push` (push the
/// image) and a `sbom-attach` (generate an SBOM for an OCI image and
/// attach it to the pushed image as an OCI referrer). All the pushes
/// come first, then all the attaches — `run_effects` runs the list in
/// order, so an attach always runs after its registry's push has
/// landed the image it will hang the SBOM off. A `sbom-attach` against
/// a non-OCI job self-skips at run time.
///
/// Config validation (`validate_references`) guarantees every name
/// resolves; an unresolved name here is treated defensively as a skip
/// rather than a panic.
pub fn registry_push_effects(
    config: &argunix_config::Config,
    repo: &argunix_config::Repo,
) -> Vec<Arc<dyn Effect>> {
    let mut pushes: Vec<Arc<dyn Effect>> = Vec::new();
    let mut attaches: Vec<Arc<dyn Effect>> = Vec::new();
    for name in &repo.push_to_registries {
        let Some(reg) = config.registries.get(name) else {
            tracing::warn!(
                registry = %name,
                repo = %repo.slug.as_str(),
                "push_to_registries names an unknown registry; skipping",
            );
            continue;
        };
        let auth_path = reg.auth_path.as_ref().map(|p| p.path().to_path_buf());
        pushes.push(Arc::new(argunix_effects::RegistryPush {
            target: name.clone(),
            registry_url: reg.url.clone(),
            namespace: reg.namespace.clone(),
            auth_path: auth_path.clone(),
            insecure: reg.insecure,
        }));
        attaches.push(Arc::new(argunix_effects::SbomAttach {
            target: name.clone(),
            registry_url: reg.url.clone(),
            namespace: reg.namespace.clone(),
            auth_path,
            insecure: reg.insecure,
        }));
    }
    pushes.extend(attaches);
    pushes
}

/// One effect's outcome, returned by [`run_effects`] so the caller can
/// surface `Reported` effects as forge checks. The `effect_runs` rows
/// are already written inside `run_effects`.
pub struct EffectReport {
    pub kind: &'static str,
    pub target: String,
    pub severity: Severity,
    pub status: EffectStatus,
    pub detail: String,
}

/// Run every effect in `effects` against `ctx`, recording an
/// `effect_runs` row per attempt. The row is inserted `running`
/// *before* the effect runs, so an effect that hangs or a daemon that
/// dies mid-effect still leaves a visible row. Returns one
/// [`EffectReport`] per effect, in order.
pub async fn run_effects(
    store: &SqlxStore,
    job_id: JobId,
    effects: &[Arc<dyn Effect>],
    ctx: &OutputContext<'_>,
) -> Vec<EffectReport> {
    let mut reports = Vec::with_capacity(effects.len());
    for effect in effects {
        let run_id = match store
            .create_effect_run(job_id, effect.kind(), effect.target(), Utc::now())
            .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    kind = effect.kind(),
                    "effect_runs insert failed; effect will run untracked",
                );
                None
            }
        };

        let outcome = effect.run(ctx).await;
        match outcome.status {
            EffectStatus::Success => tracing::info!(
                job_id = job_id.get(),
                kind = effect.kind(),
                target = effect.target(),
                detail = %outcome.detail,
                "effect succeeded",
            ),
            EffectStatus::Skipped => tracing::debug!(
                job_id = job_id.get(),
                kind = effect.kind(),
                target = effect.target(),
                detail = %outcome.detail,
                "effect skipped",
            ),
            EffectStatus::Failure => tracing::warn!(
                job_id = job_id.get(),
                kind = effect.kind(),
                target = effect.target(),
                detail = %outcome.detail,
                "effect failed; job stays success",
            ),
        }

        if let Some(id) = run_id {
            if let Err(e) = store
                .finish_effect_run(
                    id,
                    outcome.status.as_str(),
                    Some(&outcome.detail),
                    Utc::now(),
                )
                .await
            {
                tracing::warn!(error = %e, "effect_runs finish update failed");
            }
        }

        reports.push(EffectReport {
            kind: effect.kind(),
            target: effect.target().to_string(),
            severity: effect.severity(),
            status: outcome.status,
            detail: outcome.detail,
        });
    }
    reports
}

/// Push a successful build's output closure to every configured binary
/// cache, recording one `effect_runs` row per cache.
///
/// Binary-cache push is itself a post-build effect — first-class in
/// `effect_runs` alongside `registry-push` — even though the `nix copy`
/// mechanics stay in `argunix-build` (it is deeply nix-native, where a
/// registry push is not). Returns the total wall-clock so the caller
/// can also stamp the legacy aggregate `jobs.cache_push_ms` column the
/// UI already renders.
pub async fn run_cache_push_effects(
    store: &SqlxStore,
    job_id: JobId,
    output_paths: &[String],
    caches: &[argunix_build::PushCache],
    per_cache_timeout: Duration,
) -> Duration {
    let started = std::time::Instant::now();
    for cache in caches {
        let run_id = match store
            .create_effect_run(job_id, "cache-push", &cache.url, Utc::now())
            .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(error = %e, "effect_runs insert failed; cache push runs untracked");
                None
            }
        };

        let (status, detail) = match argunix_build::push_one_to_cache(
            output_paths,
            cache,
            per_cache_timeout,
        )
        .await
        {
            Ok(()) => {
                tracing::info!(job_id = job_id.get(), cache = %cache.url, "cache push succeeded");
                ("success", format!("pushed output closure to {}", cache.url))
            }
            Err(e) => {
                tracing::warn!(
                    job_id = job_id.get(),
                    cache = %cache.url,
                    error = %e,
                    "cache push failed; job stays success",
                );
                ("failure", e.to_string())
            }
        };

        if let Some(id) = run_id {
            if let Err(e) = store
                .finish_effect_run(id, status, Some(&detail), Utc::now())
                .await
            {
                tracing::warn!(error = %e, "effect_runs finish update failed");
            }
        }
    }
    started.elapsed()
}

/// Convenience for the worker's detached cache-push task: run the cache
/// pushes, record per-cache `effect_runs`, then stamp the aggregate
/// `jobs.cache_push_ms`.
pub async fn cache_push_and_record(
    store: &SqlxStore,
    job_id: JobId,
    output_paths: &[String],
    caches: &[argunix_build::PushCache],
    per_cache_timeout: Duration,
) {
    let elapsed = run_cache_push_effects(store, job_id, output_paths, caches, per_cache_timeout)
        .await
        .as_millis() as u64;
    if let Err(e) = <SqlxStore as JobStore>::record_cache_push_ms(store, job_id, elapsed).await {
        tracing::warn!(
            job_id = job_id.get(),
            error = %e,
            "failed to record cache_push_ms; row still shows blank",
        );
    }
}
