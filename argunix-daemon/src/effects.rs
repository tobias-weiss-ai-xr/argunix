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
use argunix_effects::{Effect, EffectStatus, OutputContext};
use argunix_store::{EffectRunStore, JobStore, SqlxStore};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;

/// Build the `registry-push` effects that apply to `repo`, resolving
/// each `push_to_registries` name against the global `registries`
/// catalog. Config validation (`validate_references`) guarantees every
/// name resolves; an unresolved name here is treated defensively as a
/// skip rather than a panic.
pub fn registry_push_effects(
    config: &argunix_config::Config,
    repo: &argunix_config::Repo,
) -> Vec<Arc<dyn Effect>> {
    let mut out: Vec<Arc<dyn Effect>> = Vec::new();
    for name in &repo.push_to_registries {
        let Some(reg) = config.registries.get(name) else {
            tracing::warn!(
                registry = %name,
                repo = %repo.slug.as_str(),
                "push_to_registries names an unknown registry; skipping",
            );
            continue;
        };
        out.push(Arc::new(argunix_effects::RegistryPush {
            target: name.clone(),
            registry_url: reg.url.clone(),
            namespace: reg.namespace.clone(),
            auth_path: reg.auth_path.as_ref().map(|p| p.path().to_path_buf()),
            insecure: reg.insecure,
        }));
    }
    out
}

/// Run every effect in `effects` against `ctx`, recording an
/// `effect_runs` row per attempt. The row is inserted `running`
/// *before* the effect runs, so an effect that hangs or a daemon that
/// dies mid-effect still leaves a visible row.
pub async fn run_effects(
    store: &SqlxStore,
    job_id: JobId,
    effects: &[Arc<dyn Effect>],
    ctx: &OutputContext<'_>,
) {
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
    }
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
