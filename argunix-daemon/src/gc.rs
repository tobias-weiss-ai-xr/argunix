//! Retention GC.
//!
//! Background ticker that prunes terminal evaluations and their on-disk
//! state (log dirs + GC root symlinks) per the YAML retention rules.
//! Two passes per tick:
//!
//! 1. **Age**: per-repo, drop terminal evals whose `finished_at` is older
//!    than `effective_max_age_days(repo, global)`.
//! 2. **Size**: global, while the log directory exceeds `max_size_gb`,
//!    drop the oldest terminal evals across all repos until under budget.
//!
//! Non-terminal evals (queued / evaluating / building) are never deleted
//! — those are the redrive bug's territory, not retention's. GC roots
//! are deleted as symlinks only; nix's automatic GC reclaims the store
//! path eventually. See [docs/concepts/gc-roots.md] for the broader
//! retention model.
//!
//! Filesystem failures log a warning and continue, matching the posture
//! of `prune_orphan_state` at startup.
//!
//! `tick` is split out from `run` so unit tests can drive a single pass
//! against a synthetic store + log tree with an injected clock.

use argunix_config::Config;
use argunix_domain::{EvalId, RepoId};
use argunix_store::{EvalStore, RepoStore, SqlxStore};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Wiring for the retention task. Cloned out of `serve()`'s state next
/// to the worker / control / builder spawns.
pub struct GcContext {
    pub current: Arc<arc_swap::ArcSwap<argunix_web::ConfigSnapshot>>,
    pub store: SqlxStore,
    pub log_dir: PathBuf,
    pub gc_root_dir: PathBuf,
}

/// One pass's outcome. Returned for tests; the run loop logs it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickStats {
    pub age_deleted: u64,
    pub size_deleted: u64,
    pub bytes_freed: u64,
    pub bytes_remaining: u64,
}

pub fn spawn(ctx: GcContext) -> JoinHandle<()> {
    tokio::spawn(async move { run(ctx).await })
}

async fn run(ctx: GcContext) {
    loop {
        // Snapshot at the top of each tick. Reload swaps the config
        // atomically, so a tick that started under the old retention
        // settings always finishes under them.
        let snap = ctx.current.load_full();
        let interval =
            Duration::from_secs(u64::from(snap.config.retention.interval_minutes.max(1)) * 60);
        // Sleep first: don't pile a tree walk onto an already-busy boot.
        tokio::time::sleep(interval).await;

        let snap = ctx.current.load_full();
        let stats = tick(
            &snap.config,
            &ctx.store,
            &ctx.log_dir,
            &ctx.gc_root_dir,
            Utc::now(),
        )
        .await;
        if stats.age_deleted > 0 || stats.size_deleted > 0 {
            tracing::info!(
                age_deleted = stats.age_deleted,
                size_deleted = stats.size_deleted,
                bytes_freed = stats.bytes_freed,
                bytes_remaining = stats.bytes_remaining,
                "retention pass complete",
            );
        } else {
            tracing::debug!(
                bytes_remaining = stats.bytes_remaining,
                "retention pass: nothing to prune",
            );
        }
    }
}

/// One full pass. Public for unit tests; the run loop drives it from a
/// snapshot.
pub async fn tick(
    config: &Config,
    store: &SqlxStore,
    log_dir: &Path,
    gc_root_dir: &Path,
    now: DateTime<Utc>,
) -> TickStats {
    let mut stats = TickStats::default();

    // ── 1. Age pass. Per-repo, applies the effective `max_age_days`.
    for repo in &config.repos {
        let Some(days) = repo.effective_max_age_days(&config.retention) else {
            continue;
        };
        let cutoff = now - chrono::Duration::days(i64::from(days));
        let repo_id = match <SqlxStore as RepoStore>::find(store, &repo.forge, &repo.slug).await {
            Ok(Some(r)) => r.id,
            Ok(None) => continue, // repo not yet in DB — nothing to prune
            Err(e) => {
                tracing::warn!(error = %e, forge = %repo.forge, slug = %repo.slug, "retention: repo lookup failed");
                continue;
            }
        };
        let stale = match <SqlxStore as EvalStore>::list_terminal_evals_older_than(
            store, repo_id, cutoff,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, repo_id = repo_id.get(), "retention: age picker failed");
                continue;
            }
        };
        for eval in stale {
            let freed = delete_eval(eval.id, eval.repo_id, store, log_dir, gc_root_dir).await;
            stats.age_deleted += 1;
            stats.bytes_freed += freed;
        }
    }

    // ── 2. Size pass. Global cap on the log dir on disk.
    let mut current_bytes = dir_size(log_dir).await.unwrap_or(0);
    if let Some(max_gb) = config.retention.max_size_gb {
        let cap = max_gb.saturating_mul(1024 * 1024 * 1024);
        // Pull oldest-first in batches so a tick over a deeply oversized
        // tree doesn't have to load every eval into memory at once.
        const BATCH: u32 = 64;
        while current_bytes > cap {
            let batch = match <SqlxStore as EvalStore>::list_terminal_evals_oldest_first(
                store, BATCH,
            )
            .await
            {
                Ok(v) if v.is_empty() => break,
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "retention: size picker failed");
                    break;
                }
            };
            let mut deleted_in_batch = 0;
            for eval in batch {
                if current_bytes <= cap {
                    break;
                }
                let freed = delete_eval(eval.id, eval.repo_id, store, log_dir, gc_root_dir).await;
                stats.size_deleted += 1;
                stats.bytes_freed += freed;
                current_bytes = current_bytes.saturating_sub(freed);
                deleted_in_batch += 1;
            }
            // No progress (e.g. every eval's log dir is already missing
            // and the on-disk size is dominated by orphan files we can't
            // attribute) → don't loop forever. Bail with a warning so the
            // operator notices.
            if deleted_in_batch == 0 {
                tracing::warn!(
                    bytes = current_bytes,
                    cap,
                    "retention: size pass made no progress despite remaining evals; manual cleanup likely needed",
                );
                break;
            }
        }
    }
    stats.bytes_remaining = current_bytes;
    stats
}

/// Cascade-delete one eval's DB rows + on-disk state. Returns the
/// number of bytes freed from the log directory (best-effort — 0 on
/// any error or missing path). Failures of either step are logged but
/// non-fatal: the goal is "make as much progress as we can per tick".
async fn delete_eval(
    eval_id: EvalId,
    repo_id: RepoId,
    store: &SqlxStore,
    log_dir: &Path,
    gc_root_dir: &Path,
) -> u64 {
    let log_path = log_dir
        .join(repo_id.get().to_string())
        .join(eval_id.get().to_string());
    let gc_path = gc_root_dir
        .join(repo_id.get().to_string())
        .join(eval_id.get().to_string());

    // Measure before removing so we can return bytes freed without
    // re-walking the whole tree.
    let bytes = dir_size(&log_path).await.unwrap_or(0);

    // DB first. If this fails we keep the files: a future tick will
    // try again. The opposite ordering would risk surfacing a 404 for
    // a still-listed eval.
    if let Err(e) = <SqlxStore as EvalStore>::delete_eval_cascade(store, eval_id).await {
        tracing::warn!(error = %e, eval_id = eval_id.get(), "retention: cascade-delete failed");
        return 0;
    }

    if let Err(e) = tokio::fs::remove_dir_all(&log_path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, dir = %log_path.display(), "retention: log dir removal failed");
        }
    }
    if let Err(e) = tokio::fs::remove_dir_all(&gc_path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(error = %e, dir = %gc_path.display(), "retention: gcroot dir removal failed");
        }
    }
    bytes
}

/// Recursive on-disk size. Returns 0 if the path doesn't exist (the
/// log dir is created lazily on first build, so a fresh deployment
/// won't have one yet). Walked iteratively to avoid stack growth on
/// deep trees.
async fn dir_size(root: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use argunix_config::{
        BinaryCache, CloneConfig, EvalDefaults, EvalOverrides, ForgeConfig, Repo, RepoRetention,
        Retention, Schedule, WebConfig,
    };
    use argunix_domain::{AttrPath, EvalStatus, ForgeKind, Sha, Slug};
    use argunix_store::{JobStore, NewEvaluation, NewJob};
    use std::collections::BTreeMap;

    async fn fresh_store() -> SqlxStore {
        let pool = argunix_store::open_in_memory().await.unwrap();
        SqlxStore::new(pool)
    }

    fn cfg(
        repos: Vec<Repo>,
        forges: BTreeMap<String, ForgeConfig>,
        retention: Retention,
    ) -> Config {
        Config {
            external_url: "https://argunix.example.com".into(),
            listen: "127.0.0.1:8080".into(),
            control_socket: "/tmp/argunix-test.sock".into(),
            dry_run: false,
            schedule: Schedule::default(),
            retention,
            eval: EvalDefaults::default(),
            web: WebConfig::default(),
            forges,
            binary_caches: Vec::<BinaryCache>::new(),
            repos,
            builder_enrollment: None,
        }
    }

    fn repo(forge: &str, slug: &str, retention: RepoRetention) -> Repo {
        Repo {
            slug: Slug::new(slug).unwrap(),
            forge: forge.into(),
            watched_branches: vec!["main".into()],
            build_prs: true,
            pr_allowlist: vec![],
            clone: CloneConfig::default(),
            eval: EvalOverrides::default(),
            collapsed_check_threshold: None,
            weight: 1,
            retention,
        }
    }

    fn forge_entry() -> (String, ForgeConfig) {
        (
            "gh".into(),
            ForgeConfig {
                kind: ForgeKind::Github,
                web_url: "https://github.com".into(),
                token_path: None,
                app_id: None,
                app_private_key_path: None,
            },
        )
    }

    /// Create a finished eval *and* drop a fake log file at the
    /// expected path so size measurement is exercised end-to-end. Pads
    /// the file out to `payload_bytes` so size-pass ordering can be
    /// asserted deterministically.
    async fn finished_eval_with_log(
        store: &SqlxStore,
        log_dir: &Path,
        repo_id: RepoId,
        sha_pad: char,
        status: EvalStatus,
        finished_at: DateTime<Utc>,
        payload_bytes: usize,
    ) -> EvalId {
        let eval_id = <SqlxStore as EvalStore>::create(
            store,
            NewEvaluation {
                repo_id,
                trigger: "push".into(),
                git_ref: "refs/heads/main".into(),
                sha: Sha::new(sha_pad.to_string().repeat(40)).unwrap(),
                pr_number: None,
            },
        )
        .await
        .unwrap();
        let job_id = <SqlxStore as JobStore>::create(
            store,
            NewJob {
                eval_id,
                attr_path: AttrPath::new("packages.x86_64-linux.demo"),
                drv_path: None,
                system: "x86_64-linux".into(),
                main_program: None,
                outputs: Default::default(),
            },
        )
        .await
        .unwrap();
        <SqlxStore as EvalStore>::finish(store, eval_id, status, finished_at)
            .await
            .unwrap();

        // Layout matches `worker.rs:881-884`: <log_dir>/<repo_id>/<eval_id>/<job_id>.log.zst
        let dir = log_dir
            .join(repo_id.get().to_string())
            .join(eval_id.get().to_string());
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join(format!("{}.log.zst", job_id.get()));
        tokio::fs::write(&path, vec![0u8; payload_bytes])
            .await
            .unwrap();
        eval_id
    }

    #[tokio::test]
    async fn age_pass_drops_old_terminal_evals_only() {
        let s = fresh_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("logs");
        let gc_dir = tmp.path().join("gc");
        let now = Utc::now();
        let day = chrono::Duration::days(1);

        let (forge, fc) = forge_entry();
        let mut forges = BTreeMap::new();
        forges.insert(forge.clone(), fc);
        let repos = vec![repo(&forge, "alice/proj", RepoRetention::default())];
        let config = cfg(
            repos,
            forges,
            Retention {
                max_age_days: Some(7),
                max_size_gb: None,
                interval_minutes: 60,
            },
        );
        let repo_id = <SqlxStore as RepoStore>::upsert(&s, "gh", &Slug::new("alice/proj").unwrap())
            .await
            .unwrap();

        let stale = finished_eval_with_log(
            &s,
            &log_dir,
            repo_id,
            '1',
            EvalStatus::Done,
            now - day * 30,
            128,
        )
        .await;
        let recent = finished_eval_with_log(
            &s,
            &log_dir,
            repo_id,
            '2',
            EvalStatus::Done,
            now - day * 2,
            128,
        )
        .await;
        // Non-terminal: must survive even though it has no `finished_at`.
        let active = <SqlxStore as EvalStore>::create(
            &s,
            NewEvaluation {
                repo_id,
                trigger: "push".into(),
                git_ref: "refs/heads/main".into(),
                sha: Sha::new("3".repeat(40)).unwrap(),
                pr_number: None,
            },
        )
        .await
        .unwrap();
        <SqlxStore as EvalStore>::set_status(&s, active, EvalStatus::Building)
            .await
            .unwrap();

        let stats = tick(&config, &s, &log_dir, &gc_dir, now).await;
        assert_eq!(stats.age_deleted, 1);
        assert_eq!(stats.size_deleted, 0);

        // Stale eval row + log file gone.
        assert!(
            <SqlxStore as EvalStore>::get(&s, stale)
                .await
                .unwrap()
                .is_none()
        );
        let stale_dir = log_dir
            .join(repo_id.get().to_string())
            .join(stale.get().to_string());
        assert!(!stale_dir.exists(), "stale eval log dir should be removed");

        // Recent + non-terminal preserved.
        assert!(
            <SqlxStore as EvalStore>::get(&s, recent)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            <SqlxStore as EvalStore>::get(&s, active)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn per_repo_override_wins_over_global() {
        let s = fresh_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("logs");
        let gc_dir = tmp.path().join("gc");
        let now = Utc::now();
        let day = chrono::Duration::days(1);

        let (forge, fc) = forge_entry();
        let mut forges = BTreeMap::new();
        forges.insert(forge.clone(), fc);
        // Global = 30 days (lenient), one repo overrides to 3 days
        // (aggressive). A 5-day-old eval on the strict repo should be
        // pruned; the same age on the lenient repo should survive.
        let repos = vec![
            repo(&forge, "lenient/proj", RepoRetention::default()),
            repo(
                &forge,
                "strict/proj",
                RepoRetention {
                    max_age_days: Some(3),
                },
            ),
        ];
        let config = cfg(
            repos,
            forges,
            Retention {
                max_age_days: Some(30),
                max_size_gb: None,
                interval_minutes: 60,
            },
        );
        let lenient =
            <SqlxStore as RepoStore>::upsert(&s, "gh", &Slug::new("lenient/proj").unwrap())
                .await
                .unwrap();
        let strict = <SqlxStore as RepoStore>::upsert(&s, "gh", &Slug::new("strict/proj").unwrap())
            .await
            .unwrap();
        let lenient_eval = finished_eval_with_log(
            &s,
            &log_dir,
            lenient,
            '1',
            EvalStatus::Done,
            now - day * 5,
            32,
        )
        .await;
        let strict_eval = finished_eval_with_log(
            &s,
            &log_dir,
            strict,
            '2',
            EvalStatus::Done,
            now - day * 5,
            32,
        )
        .await;

        let stats = tick(&config, &s, &log_dir, &gc_dir, now).await;
        assert_eq!(stats.age_deleted, 1);
        assert!(
            <SqlxStore as EvalStore>::get(&s, lenient_eval)
                .await
                .unwrap()
                .is_some(),
            "5-day eval on 30-day repo must survive",
        );
        assert!(
            <SqlxStore as EvalStore>::get(&s, strict_eval)
                .await
                .unwrap()
                .is_none(),
            "5-day eval on 3-day repo must be pruned",
        );
    }

    #[tokio::test]
    async fn size_pass_evicts_oldest_until_under_cap() {
        let s = fresh_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("logs");
        let gc_dir = tmp.path().join("gc");
        let now = Utc::now();
        let hour = chrono::Duration::hours(1);

        let (forge, fc) = forge_entry();
        let mut forges = BTreeMap::new();
        forges.insert(forge.clone(), fc);
        let repos = vec![repo(&forge, "a/b", RepoRetention::default())];
        // Cap: 0 GB so anything triggers the size pass; we expect every
        // terminal eval to be cleared. (1 GB granularity is the schema's
        // smallest unit; finer units would just complicate the YAML.)
        let config = cfg(
            repos,
            forges,
            Retention {
                max_age_days: None,
                max_size_gb: Some(0),
                interval_minutes: 60,
            },
        );
        let repo_id = <SqlxStore as RepoStore>::upsert(&s, "gh", &Slug::new("a/b").unwrap())
            .await
            .unwrap();

        // Three terminal evals at increasing finish times.
        let oldest = finished_eval_with_log(
            &s,
            &log_dir,
            repo_id,
            '1',
            EvalStatus::Done,
            now - hour * 5,
            1024,
        )
        .await;
        let middle = finished_eval_with_log(
            &s,
            &log_dir,
            repo_id,
            '2',
            EvalStatus::Done,
            now - hour * 3,
            1024,
        )
        .await;
        let newest = finished_eval_with_log(
            &s,
            &log_dir,
            repo_id,
            '3',
            EvalStatus::Done,
            now - hour,
            1024,
        )
        .await;

        let stats = tick(&config, &s, &log_dir, &gc_dir, now).await;
        assert_eq!(stats.size_deleted, 3);
        for id in [oldest, middle, newest] {
            assert!(
                <SqlxStore as EvalStore>::get(&s, id)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        // bytes_remaining is what `dir_size` saw at the start of the
        // pass minus what we attributed to deleted evals. With
        // payload-only files the residual should be near zero.
        assert_eq!(stats.bytes_remaining, 0);
    }

    #[tokio::test]
    async fn no_retention_configured_is_a_full_no_op() {
        let s = fresh_store().await;
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("logs");
        let gc_dir = tmp.path().join("gc");
        let now = Utc::now();

        let (forge, fc) = forge_entry();
        let mut forges = BTreeMap::new();
        forges.insert(forge.clone(), fc);
        let repos = vec![repo(&forge, "a/b", RepoRetention::default())];
        let config = cfg(repos, forges, Retention::default()); // both caps = None

        let repo_id = <SqlxStore as RepoStore>::upsert(&s, "gh", &Slug::new("a/b").unwrap())
            .await
            .unwrap();
        let eval = finished_eval_with_log(
            &s,
            &log_dir,
            repo_id,
            '1',
            EvalStatus::Done,
            now - chrono::Duration::days(365 * 5),
            64,
        )
        .await;

        let stats = tick(&config, &s, &log_dir, &gc_dir, now).await;
        assert_eq!(stats.age_deleted, 0);
        assert_eq!(stats.size_deleted, 0);
        assert!(
            <SqlxStore as EvalStore>::get(&s, eval)
                .await
                .unwrap()
                .is_some(),
            "no caps = nothing pruned even for ancient evals",
        );
    }

    #[tokio::test]
    async fn dir_size_handles_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("nope");
        assert_eq!(dir_size(&absent).await.unwrap(), 0);
    }
}
