//! Retention GC.
//!
//! Background ticker that prunes terminal evaluations and their on-disk
//! state (log dirs + GC root symlinks) per the YAML retention rules.
//! Two passes per tick:
//!
//! 1. **Age**: per-repo, drop terminal evals whose `finished_at` is older
//!    than `effective_max_age_days(repo, global)`.
//! 2. **Size**: global, while the store closure pinned by argunix's
//!    gcroots exceeds `max_size_gb`, drop the oldest terminal evals
//!    across all repos until under budget. The closure footprint —
//!    not the log directory — is what matters for disk: log files
//!    are zstd-compressed log lines, build outputs are sometimes
//!    gigabytes of OCI image layers.
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
//! against a synthetic store + log tree with an injected clock and a
//! stubbed [`RootedStoreSizer`].

use argunix_config::Config;
use argunix_domain::{EvalId, RepoId};
use argunix_store::{EvalStore, RepoStore, SqlxStore};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinHandle;

/// Source of "how many bytes is argunix pinning?" measurements.
/// Pluggable so tests can drive the size pass deterministically
/// without needing a real /nix/store. Production is [`NixStoreSizer`].
#[async_trait]
pub trait RootedStoreSizer: Send + Sync {
    /// Sum of NAR sizes of the unique store paths reachable through
    /// gcroot symlinks planted under `gc_root_dir`. Returns 0 on any
    /// hard failure (missing nix binary, permission error, etc.) so
    /// the retention loop stays best-effort.
    async fn rooted_bytes(&self, gc_root_dir: &Path) -> u64;
}

/// Production sizer. Shells out to `nix-store --query` twice:
/// first to expand each gcroot target into its closure, then to ask
/// for per-path NAR sizes. Both invocations are chunked so a wide
/// gcroot tree doesn't exceed `ARG_MAX`.
pub struct NixStoreSizer {
    pub nix_store_bin: PathBuf,
}

/// Wiring for the retention task. Cloned out of `serve()`'s state next
/// to the worker / control / builder spawns.
pub struct GcContext {
    pub current: Arc<arc_swap::ArcSwap<argunix_web::ConfigSnapshot>>,
    pub store: SqlxStore,
    pub log_dir: PathBuf,
    pub gc_root_dir: PathBuf,
    pub sizer: Arc<dyn RootedStoreSizer>,
}

/// One pass's outcome. Returned for tests; the run loop logs it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickStats {
    pub age_deleted: u64,
    pub size_deleted: u64,
    /// Bytes of rooted store closure that became unreachable across
    /// this pass. Computed as `rooted_bytes(pre) - rooted_bytes(post)`
    /// around each size-pass batch, so closures shared across evals
    /// don't double-count.
    pub bytes_freed: u64,
    /// Rooted store bytes still pinned at end of pass.
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
            ctx.sizer.as_ref(),
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
    sizer: &dyn RootedStoreSizer,
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
            let _ = delete_eval(eval.id, eval.repo_id, store, log_dir, gc_root_dir).await;
            stats.age_deleted += 1;
        }
    }

    // ── 2. Size pass. Global cap on the rooted store closure. We measure
    // once at the start, again after each batch of deletions, and use
    // the difference for `bytes_freed` so closures shared across evals
    // don't double-count.
    let mut current_bytes = sizer.rooted_bytes(gc_root_dir).await;
    if let Some(max_gb) = config.retention.max_size_gb {
        let cap = max_gb.saturating_mul(1024 * 1024 * 1024);
        // Batches are small so we don't over-evict by much between
        // remeasurements: deleting ~8 evals' worth of gcroots reclaims
        // a meaningful chunk without skipping the cap by a week of
        // builds.
        const BATCH: u32 = 8;
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
            let mut deleted_in_batch: u64 = 0;
            for eval in batch {
                let _ = delete_eval(eval.id, eval.repo_id, store, log_dir, gc_root_dir).await;
                stats.size_deleted += 1;
                deleted_in_batch += 1;
            }
            let after = sizer.rooted_bytes(gc_root_dir).await;
            let freed = current_bytes.saturating_sub(after);
            stats.bytes_freed = stats.bytes_freed.saturating_add(freed);
            current_bytes = after;
            // No progress (eval rows exist but their gcroots were
            // already missing, or the sizer is broken) → don't loop
            // forever. Bail with a warning so the operator notices.
            if freed == 0 && deleted_in_batch > 0 {
                tracing::warn!(
                    bytes = current_bytes,
                    cap,
                    deleted_in_batch,
                    "retention: size pass deleted evals but freed no rooted bytes; \
                     gcroots may already be gone or sizer is misconfigured",
                );
                break;
            }
        }
    }
    stats.bytes_remaining = current_bytes;
    stats
}

/// Cascade-delete one eval's DB rows + on-disk state. Failures of
/// either step are logged but non-fatal: the goal is "make as much
/// progress as we can per tick".
async fn delete_eval(
    eval_id: EvalId,
    repo_id: RepoId,
    store: &SqlxStore,
    log_dir: &Path,
    gc_root_dir: &Path,
) {
    let log_path = log_dir
        .join(repo_id.get().to_string())
        .join(eval_id.get().to_string());
    let gc_path = gc_root_dir
        .join(repo_id.get().to_string())
        .join(eval_id.get().to_string());

    // DB first. If this fails we keep the files: a future tick will
    // try again. The opposite ordering would risk surfacing a 404 for
    // a still-listed eval.
    if let Err(e) = <SqlxStore as EvalStore>::delete_eval_cascade(store, eval_id).await {
        tracing::warn!(error = %e, eval_id = eval_id.get(), "retention: cascade-delete failed");
        return;
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
}

/// Recursive on-disk size. Returns 0 if the path doesn't exist (the
/// log dir is created lazily on first build, so a fresh deployment
/// won't have one yet). Walked iteratively to avoid stack growth on
/// deep trees. Kept around because the test infrastructure still uses
/// it to drive a synthetic [`FakeSizer`].
#[cfg(test)]
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

/// Walk `gc_root_dir` and resolve every symlink it contains to its
/// store-path target. Non-symlinks (intermediate `<repo>/<eval>/`
/// directories) recurse; dangling symlinks are skipped silently.
async fn collect_gcroot_targets(gc_root_dir: &Path) -> Vec<PathBuf> {
    let mut targets: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![gc_root_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    dir = %dir.display(),
                    "rooted_bytes: read_dir failed",
                );
                continue;
            }
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let ft = match entry.file_type().await {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                if let Ok(target) = tokio::fs::canonicalize(entry.path()).await {
                    if target.starts_with("/nix/store") {
                        targets.push(target);
                    }
                }
            } else if ft.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    targets
}

/// Spawn `nix-store <args> <chunk>` and capture stdout. Hard failures
/// (binary missing, non-zero exit) log a warning and return None.
async fn run_nix_store(bin: &Path, args: &[&str], chunk: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.args(chunk);
    cmd.stdin(Stdio::null()).stderr(Stdio::piped());
    let out = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = %e, bin = %bin.display(), "rooted_bytes: spawn failed");
            return None;
        }
    };
    if !out.status.success() {
        tracing::warn!(
            bin = %bin.display(),
            args = ?args,
            status = ?out.status.code(),
            stderr = %String::from_utf8_lossy(&out.stderr),
            "rooted_bytes: nix-store returned non-zero",
        );
        return None;
    }
    Some(out.stdout)
}

/// Chunk size for nix-store arg lists. Picked so a chunk's total argv
/// stays well under typical `ARG_MAX` (2 MiB on Linux): 200 store paths
/// × ~120 chars each ≈ 24 KiB.
const NIX_CHUNK: usize = 200;

#[async_trait]
impl RootedStoreSizer for NixStoreSizer {
    async fn rooted_bytes(&self, gc_root_dir: &Path) -> u64 {
        let targets = collect_gcroot_targets(gc_root_dir).await;
        if targets.is_empty() {
            return 0;
        }
        let target_strs: Vec<String> = targets
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        // Step 1: expand each gcroot target's transitive closure. The
        // output is naturally deduplicated within one invocation; we
        // dedupe across chunks ourselves.
        let mut closure: HashSet<String> = HashSet::new();
        for chunk in target_strs.chunks(NIX_CHUNK) {
            let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
            let Some(stdout) =
                run_nix_store(&self.nix_store_bin, &["--query", "--requisites"], &refs).await
            else {
                return 0;
            };
            for line in String::from_utf8_lossy(&stdout).lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    closure.insert(trimmed.to_string());
                }
            }
        }
        if closure.is_empty() {
            return 0;
        }

        // Step 2: ask nix for each closure-path's NAR size and sum.
        let closure_vec: Vec<String> = closure.into_iter().collect();
        let mut total: u64 = 0;
        for chunk in closure_vec.chunks(NIX_CHUNK) {
            let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
            let Some(stdout) =
                run_nix_store(&self.nix_store_bin, &["--query", "--size"], &refs).await
            else {
                return 0;
            };
            for line in String::from_utf8_lossy(&stdout).lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(b) = trimmed.parse::<u64>() {
                    total = total.saturating_add(b);
                }
            }
        }
        total
    }
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
            registries: BTreeMap::new(),
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
            push_to_registries: Vec::new(),
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

    /// Test sizer that returns `bytes_per_eval × (number of `<repo>/<eval>`
    /// subdirs present under `gc_root_dir`). Drives the size pass
    /// deterministically: each eval represents a constant chunk of
    /// "rooted store bytes", and `delete_eval` removing the subdir
    /// decreases the next measurement by exactly one chunk.
    struct FakeSizer {
        bytes_per_eval: u64,
    }

    #[async_trait]
    impl RootedStoreSizer for FakeSizer {
        async fn rooted_bytes(&self, gc_root_dir: &Path) -> u64 {
            let mut count: u64 = 0;
            let Ok(mut repos) = tokio::fs::read_dir(gc_root_dir).await else {
                return 0;
            };
            while let Ok(Some(repo_ent)) = repos.next_entry().await {
                if !repo_ent
                    .file_type()
                    .await
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let Ok(mut evals) = tokio::fs::read_dir(repo_ent.path()).await else {
                    continue;
                };
                while let Ok(Some(eval_ent)) = evals.next_entry().await {
                    if eval_ent
                        .file_type()
                        .await
                        .map(|t| t.is_dir())
                        .unwrap_or(false)
                    {
                        count = count.saturating_add(1);
                    }
                }
            }
            count.saturating_mul(self.bytes_per_eval)
        }
    }

    /// Convenience: tests that don't exercise the size pass pass this so
    /// the sizer never trips the cap. Equivalent to "no gcroots planted".
    struct ZeroSizer;

    #[async_trait]
    impl RootedStoreSizer for ZeroSizer {
        async fn rooted_bytes(&self, _: &Path) -> u64 {
            0
        }
    }

    /// Create a finished eval *and* drop a fake log file at the
    /// expected path so size measurement is exercised end-to-end. Pads
    /// the file out to `payload_bytes` so size-pass ordering can be
    /// asserted deterministically. Also plants an empty `gc_dir/<repo>/<eval>`
    /// directory so the [`FakeSizer`] can attribute rooted bytes to it.
    async fn finished_eval_with_log(
        store: &SqlxStore,
        log_dir: &Path,
        gc_dir: &Path,
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
        // Stand-in for `gc_root::add_gc_root`: an empty directory under
        // `<gc_dir>/<repo>/<eval>` that the FakeSizer counts and that
        // `delete_eval` cleans up.
        tokio::fs::create_dir_all(
            gc_dir
                .join(repo_id.get().to_string())
                .join(eval_id.get().to_string()),
        )
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
            &gc_dir,
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
            &gc_dir,
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

        let stats = tick(&config, &s, &log_dir, &gc_dir, &ZeroSizer, now).await;
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
            &gc_dir,
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
            &gc_dir,
            strict,
            '2',
            EvalStatus::Done,
            now - day * 5,
            32,
        )
        .await;

        let stats = tick(&config, &s, &log_dir, &gc_dir, &ZeroSizer, now).await;
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

        // Three terminal evals at increasing finish times. The
        // FakeSizer attributes 2 GiB of rooted store bytes to each
        // eval gcroot dir, so the initial measurement is 6 GiB and
        // the size pass evicts oldest-first until 0.
        let two_gib: u64 = 2 * 1024 * 1024 * 1024;
        let sizer = FakeSizer {
            bytes_per_eval: two_gib,
        };
        let oldest = finished_eval_with_log(
            &s,
            &log_dir,
            &gc_dir,
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
            &gc_dir,
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
            &gc_dir,
            repo_id,
            '3',
            EvalStatus::Done,
            now - hour,
            1024,
        )
        .await;

        let stats = tick(&config, &s, &log_dir, &gc_dir, &sizer, now).await;
        assert_eq!(stats.size_deleted, 3);
        for id in [oldest, middle, newest] {
            assert!(
                <SqlxStore as EvalStore>::get(&s, id)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        // All three eval gcroot subdirs gone → sizer now returns 0,
        // and bytes_freed equals the initial 6 GiB.
        assert_eq!(stats.bytes_remaining, 0);
        assert_eq!(stats.bytes_freed, 3 * two_gib);
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
            &gc_dir,
            repo_id,
            '1',
            EvalStatus::Done,
            now - chrono::Duration::days(365 * 5),
            64,
        )
        .await;

        let stats = tick(&config, &s, &log_dir, &gc_dir, &ZeroSizer, now).await;
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
