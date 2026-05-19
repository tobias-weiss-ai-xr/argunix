//! Dev-mode UI server.
//!
//! Boots the read-only web UI against an in-memory SQLite seeded with
//! fixture data so you can iterate on HTML / Tailwind without running
//! the daemon, real builders, or any forge integration. The actual
//! routes and templates come from `argunix-web::router` — same code path
//! production hits — so what you see here is what you get in prod.
//!
//! Run with `cargo run --bin argunix-web-dev`. Hot-reload loop:
//!
//!     # terminal 1: rebuild + relaunch on every Rust/template edit
//!     cargo watch -w argunix-web -x 'run --bin argunix-web-dev'
//!
//!     # terminal 2: rebuild ui.css on every Tailwind / template edit
//!     cd argunix-web && tailwindcss -i static/input.css -o static/ui.css --watch
//!
//! Listen address: `ARGUNIX_DEV_LISTEN` env var (default `127.0.0.1:8080`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use arc_swap::ArcSwap;
use argunix_builders::{BuildPhase, BuilderRegistry, ConnState, ConnectedBuilder};
use argunix_config::Config;
use argunix_domain::{
    AttrPath, BuilderCapabilities, BuilderId, BuilderName, BuilderPubkey, EvalId, EvalStatus,
    JobId, JobStatus, RepoId, Sha, Slug,
};
use argunix_nom::{ActivityKind, NomEvent};
use argunix_store::{
    BuilderStore, EffectRunStore, EvalStore, JobPhaseMetrics, JobStore, NewBuilder, NewEvaluation,
    NewJob, RepoStore, SbomStore, SqlxStore,
};
use argunix_web::{
    AppStateInner, CancelRegistry, CoalescePool, ConfigSnapshot, HostStatsRing, LiveLogRegistry,
    PauseRegistry, spawn_host_sampler,
};
use chrono::{DateTime, TimeZone, Utc};

const FIXTURE_CONFIG_YAML: &str = r#"
external_url: https://argunix.example.com
listen: 127.0.0.1:8080
forges:
  gh:
    kind: github
    web_url: https://github.com
    token_path: /dev/null
    repos:
      argunix/argunix:
        watched_branches: [main]
      acme/widgets:
        watched_branches: [main, develop]
      acme/empty-repo: {}
  fj:
    kind: forgejo
    web_url: https://codeberg.org
    token_path: /dev/null
    repos:
      ops/infra:
        watched_branches: [main]
# Three cache entries so /cache renders all the interesting cells:
#   - asymmetric S3 + CDN (full snippets)
#   - symmetric cachix-style (full snippets, public_url == push_url)
#   - file:// with no public_url (the "incomplete" hint path).
# `signing_key_path` points at /dev/null because the dev binary never
# actually runs `nix copy`; the field is only on disk to satisfy
# `validate_secrets_exist`, which the dev binary doesn't call anyway.
binary_caches:
  - push_url: s3://argunix-cache?endpoint=https://s3.example.com&region=eu-central-1
    public_url: https://cache.example.com
    public_key: argunix-example.com:abcdefGHIJklMNOpqrstUVWXYZ0123456789abcdefGHIJk=
    signing_key_path: /dev/null
  - push_url: https://argunix.cachix.org
    public_url: https://argunix.cachix.org
    public_key: argunix.cachix.org-1:ZZZZ1234567890abcdefGHIJklMNOpqrstUVWXYZ0123abc=
    signing_key_path: /dev/null
  - push_url: file:///srv/argunix-local-cache
    signing_key_path: /dev/null
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    require_argunix_web_cwd()?;
    // `_tw` keeps the tailwind --watch child alive for the lifetime of
    // the binary; `kill_on_drop` reaps it on shutdown.
    let _tw = spawn_tailwind_watch();

    let config: Config =
        serde_yaml::from_str(FIXTURE_CONFIG_YAML).context("parsing fixture config")?;
    let pool = argunix_store::open_in_memory()
        .await
        .context("opening in-memory db")?;
    let store = SqlxStore::new(pool);

    let builder_registry = BuilderRegistry::new();
    seed(&store, &builder_registry)
        .await
        .context("seeding fixtures")?;

    let snapshot = Arc::new(ConfigSnapshot {
        config: Arc::new(config),
        // Empty providers map — real provider construction reads token
        // files from disk and the dev UI never hits the webhook handlers
        // that would dispatch through them.
        providers: Arc::new(HashMap::new()),
    });
    let current = Arc::new(ArcSwap::from(snapshot));

    let live_logs = LiveLogRegistry::new();
    // Make one running fixture job fully "live": register a build
    // phase (so its job page renders the live sections) and replay a
    // scripted nom build log onto its tap — previewable without a
    // daemon or builder pushing real chunks.
    if let Some(jwc) = <SqlxStore as JobStore>::list_running(&store).await?.first() {
        let job_id = jwc.job.id.get();
        let alpha = BuilderName::new("alpha".to_string()).expect("valid builder name");
        builder_registry.set_phase(&alpha, job_id, BuildPhase::Build);
        spawn_fixture_build_log(live_logs.clone(), job_id);
    }
    let pauses = Arc::new(PauseRegistry::new());
    let cancellations = Arc::new(CancelRegistry::new());
    let coalesce = Arc::new(CoalescePool::new(Duration::from_secs(5)));
    let host_stats = HostStatsRing::new();
    // Sampler runs as long as the dev binary does — `_host_sampler` is
    // its abort handle, kept alive so the task isn't dropped early.
    let _host_sampler = spawn_host_sampler(host_stats.clone());

    // Worker dispatcher channel. Nothing in dev mode publishes to it,
    // but `AppStateInner` requires a live sender. Keep `_keep_rx` in
    // scope so the channel stays open and lazy `tx.send` calls (e.g.
    // from a webhook hit during template fiddling) don't error.
    let (tx, _keep_rx) = tokio::sync::mpsc::unbounded_channel();

    let inner = AppStateInner {
        current,
        store,
        work_dispatcher: tx,
        coalesce,
        pauses,
        cancellations,
        builder_registry,
        live_logs,
        host_stats,
        started_at: std::time::Instant::now(),
        // Hard-coded fixture so the dev UI shows realistic values
        // without depending on the local host having `nix` and
        // `nix-eval-jobs` on PATH.
        coordinator_versions: Arc::new(argunix_web::CoordinatorVersions {
            nix_version: "2.24.10".into(),
            nix_eval_jobs_version: "2.24.0".into(),
        }),
    };
    let app_state = Arc::new(inner);
    let router = argunix_web::router(app_state);

    let listen = std::env::var("ARGUNIX_DEV_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let local = listener.local_addr().context("local addr")?;
    println!("argunix dev UI listening on http://{local}/");
    println!("    /            – cluster status (htmx-polled)");
    println!("    /repos       – repos overview");
    println!("    /hosts       – coordinator + builders");
    println!("    /cache       – binary caches + substituter snippets");
    println!("    /r/gh/argunix/argunix");
    println!("    /r/gh/acme/widgets");
    println!("    /r/fj/ops/infra");
    println!();
    println!("override port: ARGUNIX_DEV_LISTEN=127.0.0.1:9000 cargo run --bin argunix-web-dev");

    axum::serve(listener, router).await.context("serving")?;
    Ok(())
}

/// `static/input.css` and the templates live next to this binary's
/// crate root, and tailwind is invoked with relative paths — bail out
/// loud if the user ran `cargo run` from the workspace root or some
/// unrelated directory.
fn require_argunix_web_cwd() -> anyhow::Result<()> {
    if std::path::Path::new("static/input.css").is_file()
        && std::path::Path::new("templates").is_dir()
    {
        return Ok(());
    }
    eprintln!(
        "argunix-web-dev: must be run from the `argunix-web/` directory.\n\
         Current working directory: {}\n\
         Try:\n    cd argunix-web && cargo run --bin argunix-web-dev",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".into())
    );
    std::process::exit(2);
}

/// On Linux, request that the kernel send `SIGTERM` to the child if
/// the parent dies for any reason — including `SIGKILL` and panics
/// where `kill_on_drop` never fires. Without this, a hard-killed dev
/// binary leaves a tailwind --watch process orphaned to PID 1.
#[cfg(target_os = "linux")]
fn set_pdeathsig(cmd: &mut tokio::process::Command) {
    // SAFETY: `prctl` is async-signal-safe; `pre_exec` runs in the
    // child between fork and exec, and we touch nothing else here.
    unsafe {
        cmd.pre_exec(|| {
            let r = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn set_pdeathsig(_cmd: &mut tokio::process::Command) {}

/// Spawn a background task that replays a scripted `nix-output-monitor`
/// build log onto `job_id`'s live tap, on a loop — purely so the dev
/// UI's colored live log and "building now" view have something to
/// render without a daemon or real builder pushing chunks.
fn spawn_fixture_build_log(registry: Arc<LiveLogRegistry>, job_id: i64) {
    let live = registry.open(job_id);
    tokio::spawn(async move {
        loop {
            for ev in fixture_nom_script() {
                live.push(ev);
                tokio::time::sleep(Duration::from_millis(650)).await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
}

/// A canned, nom-style build: two derivations building (interleaved),
/// a download and a substitute alongside, per-derivation log lines,
/// and progress counters.
fn fixture_nom_script() -> Vec<NomEvent> {
    let line = |activity, label: &str, text: &str| NomEvent::Line {
        activity,
        label: label.into(),
        text: text.into(),
    };
    let start = |id, act, label: &str| NomEvent::ActStart {
        id,
        parent: 0,
        act,
        label: label.into(),
    };
    let progress = |done, running| NomEvent::Progress {
        done,
        expected: 3,
        running,
        failed: 0,
    };
    vec![
        progress(0, 0),
        start(
            10,
            ActivityKind::Download,
            "cache.example.com/redis.narinfo",
        ),
        NomEvent::ActStop { id: 10 },
        start(1, ActivityKind::Build, "hello-2.12.1"),
        progress(0, 1),
        line(1, "hello-2.12.1", "unpacking sources"),
        line(1, "hello-2.12.1", "configuring build system"),
        start(2, ActivityKind::Build, "libwidget-1.4.0"),
        progress(0, 2),
        line(2, "libwidget-1.4.0", "compiling widget.c"),
        line(1, "hello-2.12.1", "compiling hello.c"),
        line(2, "libwidget-1.4.0", "compiling render.c"),
        start(11, ActivityKind::Substitute, "glibc-2.40-66"),
        line(1, "hello-2.12.1", "installing into $out"),
        NomEvent::ActStop { id: 11 },
        NomEvent::ActStop { id: 1 },
        progress(1, 1),
        line(2, "libwidget-1.4.0", "running 42 tests"),
        line(2, "libwidget-1.4.0", "all tests passed"),
        NomEvent::ActStop { id: 2 },
        progress(3, 0),
        NomEvent::Message {
            level: 2,
            text: "build of 'widget-app' completed".into(),
        },
    ]
}

/// Spawn `tailwindcss --watch` so CSS edits (and template edits that
/// add new utility classes) get picked up without a server restart.
/// Returns the `Child` so the caller can keep it alive — `kill_on_drop`
/// reaps it when the dev binary exits. Returns `None` if the tailwind
/// CLI isn't on `PATH`; the server still starts (with possibly stale
/// `static/ui.css`) so that template-only iteration works in
/// environments without tailwind installed.
fn spawn_tailwind_watch() -> Option<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new("tailwindcss");
    // `--watch=always` keeps tailwind running when stdin is closed (e.g.
    // launched as a subprocess from this binary). Plain `--watch` would
    // exit immediately on EOF and never write the initial ui.css.
    cmd.args([
        "-i",
        "static/input.css",
        "-o",
        "static/ui.css",
        "--watch=always",
    ])
    .kill_on_drop(true);
    set_pdeathsig(&mut cmd);
    match cmd.spawn() {
        Ok(child) => {
            println!("tailwindcss --watch: pid {}", child.id().unwrap_or(0));
            Some(child)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "tailwindcss not found on PATH — CSS won't auto-rebuild.\n\
                 Add it via: nix shell nixpkgs#tailwindcss_4"
            );
            None
        }
        Err(e) => {
            eprintln!("failed to spawn tailwindcss --watch: {e}");
            None
        }
    }
}

// ============================================================
// Fixture seeding.
// ============================================================
//
// Goal: every interesting branch in every template should have data.
// Status page wants online/draining/offline/revoked builders, evals in
// every status, and running rows in every phase. Repo page wants a mix
// of PR / push triggers and finished/in-flight evals. Job page wants a
// terminal job with a log + drv path + phase metrics, and a live-running
// job so the SSE sparkline placeholder renders.

async fn seed(store: &SqlxStore, registry: &BuilderRegistry) -> anyhow::Result<()> {
    let t0 = Utc.with_ymd_and_hms(2026, 5, 8, 9, 0, 0).unwrap();

    seed_builders(store, registry, t0).await?;

    let argunix = upsert_repo(
        store,
        "gh",
        "argunix/argunix",
        Some("argunix"),
        Some("Multi-tenant Nix CI for forges that don't have one."),
        Some("https://github.com/argunix/argunix"),
    )
    .await?;
    let widgets = upsert_repo(
        store,
        "gh",
        "acme/widgets",
        Some("widgets"),
        Some("ACME's javascript widget library."),
        Some("https://github.com/acme/widgets"),
    )
    .await?;
    // Repo with a metadata row but no description — exercises the "—"
    // fallback in the index template.
    upsert_repo(
        store,
        "gh",
        "acme/empty-repo",
        Some("empty-repo"),
        None,
        Some("https://github.com/acme/empty-repo"),
    )
    .await?;
    let infra = upsert_repo(
        store,
        "fj",
        "ops/infra",
        Some("infra"),
        Some("Self-hosted infra flake — nixos systems, deploys, and the bastion."),
        Some("https://codeberg.org/ops/infra"),
    )
    .await?;

    seed_argunix_evals(store, argunix, t0).await?;
    seed_widgets_evals(store, widgets, t0).await?;
    seed_infra_evals(store, infra, t0).await?;

    Ok(())
}

async fn seed_builders(
    store: &SqlxStore,
    registry: &BuilderRegistry,
    t0: DateTime<Utc>,
) -> anyhow::Result<()> {
    // Two online builders, one draining, one offline-but-known, one
    // revoked. Pubkeys are dummies — the dev binary never authenticates.
    let alpha = enroll_builder(
        store,
        "alpha",
        &[1; 32],
        BuilderCapabilities {
            systems: vec!["x86_64-linux".into(), "aarch64-linux".into()],
            features: vec!["big-parallel".into(), "kvm".into(), "nixos-test".into()],
            max_jobs: 8,
            nix_version: "2.24.10".into(),
        },
        t0,
    )
    .await?;
    let beta = enroll_builder(
        store,
        "beta",
        &[2; 32],
        BuilderCapabilities {
            systems: vec!["x86_64-linux".into()],
            features: vec!["big-parallel".into(), "kvm".into()],
            max_jobs: 4,
            nix_version: "2.24.10".into(),
        },
        t0,
    )
    .await?;
    let gamma = enroll_builder(
        store,
        "gamma",
        &[3; 32],
        BuilderCapabilities {
            systems: vec!["aarch64-linux".into(), "aarch64-darwin".into()],
            features: vec![],
            max_jobs: 2,
            nix_version: "2.24.10".into(),
        },
        t0,
    )
    .await?;
    // Offline: enrolled, has a `last_seen` from two hours ago, never
    // registered into the live registry.
    let two_hours_ago = t0 - chrono::Duration::hours(2);
    let _delta = enroll_builder(
        store,
        "delta-offline",
        &[4; 32],
        BuilderCapabilities {
            systems: vec!["x86_64-linux".into()],
            features: vec!["big-parallel".into()],
            max_jobs: 4,
            nix_version: "2.22.1".into(),
        },
        two_hours_ago,
    )
    .await?;
    let epsilon = enroll_builder(
        store,
        "epsilon-old",
        &[5; 32],
        BuilderCapabilities {
            systems: vec!["x86_64-linux".into()],
            features: vec![],
            max_jobs: 2,
            nix_version: "2.18.0".into(),
        },
        t0 - chrono::Duration::days(7),
    )
    .await?;
    let _ = epsilon; // referenced below via name lookup
    <SqlxStore as BuilderStore>::revoke(store, "epsilon-old", t0 - chrono::Duration::days(2))
        .await?;

    // Push alpha + beta into the live registry as Active, gamma as
    // Disconnecting (draining). With `session: None` they're treated by
    // the status page exactly like a real connection minus dispatch
    // (which the dev UI never invokes).
    register_live(
        registry,
        "alpha",
        alpha,
        ConnState::Active,
        BuilderCapabilities {
            systems: vec!["x86_64-linux".into(), "aarch64-linux".into()],
            features: vec!["big-parallel".into(), "kvm".into(), "nixos-test".into()],
            max_jobs: 8,
            nix_version: "2.24.10".into(),
        },
        t0 - chrono::Duration::minutes(30),
    );
    register_live(
        registry,
        "beta",
        beta,
        ConnState::Active,
        BuilderCapabilities {
            systems: vec!["x86_64-linux".into()],
            features: vec!["big-parallel".into(), "kvm".into()],
            max_jobs: 4,
            nix_version: "2.24.10".into(),
        },
        t0 - chrono::Duration::minutes(45),
    );
    register_live(
        registry,
        "gamma",
        gamma,
        ConnState::Disconnecting,
        BuilderCapabilities {
            systems: vec!["aarch64-linux".into(), "aarch64-darwin".into()],
            features: vec![],
            max_jobs: 2,
            nix_version: "2.24.10".into(),
        },
        t0 - chrono::Duration::minutes(10),
    );
    Ok(())
}

async fn seed_argunix_evals(
    store: &SqlxStore,
    repo_id: RepoId,
    t0: DateTime<Utc>,
) -> anyhow::Result<()> {
    // e1: a clean Done eval on main with a mix of job outcomes.
    let e1 = create_eval(store, repo_id, "push", "main", sha("a1"), None).await?;
    let started = t0 - chrono::Duration::hours(3);
    let building = started + chrono::Duration::seconds(40);
    let finished = started + chrono::Duration::minutes(7);
    <SqlxStore as EvalStore>::start(store, e1, started, EvalStatus::Evaluating).await?;
    <SqlxStore as EvalStore>::mark_building(store, e1, building).await?;
    <SqlxStore as EvalStore>::finish(store, e1, EvalStatus::Done, finished).await?;
    seed_jobs_for_done_eval(store, e1, started, finished).await?;

    // e2: in-progress build, PR-triggered, with running + queued jobs.
    let e2 = create_eval(
        store,
        repo_id,
        "pull_request",
        "refs/pull/42/head:fix-render",
        sha("b2"),
        Some(42),
    )
    .await?;
    let started = t0 - chrono::Duration::minutes(8);
    let building = started + chrono::Duration::seconds(35);
    <SqlxStore as EvalStore>::start(store, e2, started, EvalStatus::Evaluating).await?;
    <SqlxStore as EvalStore>::mark_building(store, e2, building).await?;
    seed_jobs_for_in_flight_eval(store, e2, building).await?;

    // e3: cancelled mid-build.
    let e3 = create_eval(store, repo_id, "push", "feature/cancelled", sha("c3"), None).await?;
    let started = t0 - chrono::Duration::hours(6);
    let finished = started + chrono::Duration::minutes(2);
    <SqlxStore as EvalStore>::start(store, e3, started, EvalStatus::Evaluating).await?;
    <SqlxStore as EvalStore>::finish(store, e3, EvalStatus::Cancelled, finished).await?;

    // e4: evaluation failed with a captured reason.
    let e4 = create_eval(
        store,
        repo_id,
        "push",
        "topic/broken-flake",
        sha("d4"),
        None,
    )
    .await?;
    let started = t0 - chrono::Duration::hours(2);
    let finished = started + chrono::Duration::seconds(45);
    <SqlxStore as EvalStore>::start(store, e4, started, EvalStatus::Evaluating).await?;
    <SqlxStore as EvalStore>::fail_with_reason(
        store,
        e4,
        "nix-eval-jobs: error: attribute 'packages.x86_64-linux.foo' missing\n\
         at /nix/store/abc-source/flake.nix:42:3:\n\
         41|   outputs = inputs: {\n\
         42|     packages = ...",
        finished,
    )
    .await?;

    // e5: queued (just landed, worker hasn't picked up).
    let _e5 = create_eval(store, repo_id, "push", "main", sha("e5"), None).await?;

    Ok(())
}

async fn seed_widgets_evals(
    store: &SqlxStore,
    repo_id: RepoId,
    t0: DateTime<Utc>,
) -> anyhow::Result<()> {
    let e6 = create_eval(store, repo_id, "push", "develop", sha("f6"), None).await?;
    let started = t0 - chrono::Duration::hours(5);
    let building = started + chrono::Duration::seconds(20);
    let finished = started + chrono::Duration::minutes(4);
    <SqlxStore as EvalStore>::start(store, e6, started, EvalStatus::Evaluating).await?;
    <SqlxStore as EvalStore>::mark_building(store, e6, building).await?;
    <SqlxStore as EvalStore>::finish(store, e6, EvalStatus::Done, finished).await?;
    finish_job(
        store,
        e6,
        "packages.x86_64-linux.widget",
        "x86_64-linux",
        JobStatus::Success,
        Some("/nix/store/aaaa-widget-1.2.3"),
        true,
        Some(default_phase_metrics()),
        finished,
    )
    .await?;
    finish_job(
        store,
        e6,
        "packages.x86_64-linux.widget-tests",
        "x86_64-linux",
        JobStatus::Success,
        Some("/nix/store/bbbb-widget-tests"),
        true,
        Some(default_phase_metrics()),
        finished,
    )
    .await?;

    let e7 = create_eval(
        store,
        repo_id,
        "pull_request",
        "refs/pull/7/head:add-knob",
        sha("a7"),
        Some(7),
    )
    .await?;
    let started = t0 - chrono::Duration::hours(1);
    let building = started + chrono::Duration::seconds(25);
    let finished = started + chrono::Duration::minutes(6);
    <SqlxStore as EvalStore>::start(store, e7, started, EvalStatus::Evaluating).await?;
    <SqlxStore as EvalStore>::mark_building(store, e7, building).await?;
    <SqlxStore as EvalStore>::finish(store, e7, EvalStatus::Done, finished).await?;
    finish_job(
        store,
        e7,
        "packages.x86_64-linux.widget",
        "x86_64-linux",
        JobStatus::Success,
        Some("/nix/store/cccc-widget-1.3.0"),
        true,
        Some(default_phase_metrics()),
        finished,
    )
    .await?;
    finish_job(
        store,
        e7,
        "checks.x86_64-linux.lint",
        "x86_64-linux",
        JobStatus::Failure,
        None,
        true,
        Some(default_phase_metrics()),
        finished,
    )
    .await?;

    // e8: currently evaluating — nothing in jobs yet.
    let e8 = create_eval(store, repo_id, "push", "main", sha("88"), None).await?;
    let started = t0 - chrono::Duration::seconds(45);
    <SqlxStore as EvalStore>::start(store, e8, started, EvalStatus::Evaluating).await?;

    // e9: a done `main` build that published container images — gives
    // the eval page's "Published images" section, the job effects
    // panel, and the SBOM browser fixture data.
    let e9 = create_eval(store, repo_id, "push", "main", sha("c9"), None).await?;
    let started = t0 - chrono::Duration::hours(2);
    let building = started + chrono::Duration::seconds(30);
    let finished = started + chrono::Duration::minutes(7);
    <SqlxStore as EvalStore>::start(store, e9, started, EvalStatus::Evaluating).await?;
    <SqlxStore as EvalStore>::mark_building(store, e9, building).await?;
    <SqlxStore as EvalStore>::finish(store, e9, EvalStatus::Done, finished).await?;
    seed_image_jobs(store, e9, started, finished).await?;
    Ok(())
}

async fn seed_infra_evals(
    store: &SqlxStore,
    repo_id: RepoId,
    t0: DateTime<Utc>,
) -> anyhow::Result<()> {
    let e = create_eval(store, repo_id, "push", "main", sha("99"), None).await?;
    let started = t0 - chrono::Duration::hours(12);
    let building = started + chrono::Duration::seconds(60);
    let finished = started + chrono::Duration::minutes(15);
    <SqlxStore as EvalStore>::start(store, e, started, EvalStatus::Evaluating).await?;
    <SqlxStore as EvalStore>::mark_building(store, e, building).await?;
    <SqlxStore as EvalStore>::finish(store, e, EvalStatus::Done, finished).await?;
    finish_job(
        store,
        e,
        "nixosConfigurations.bastion",
        "x86_64-linux",
        JobStatus::Success,
        Some("/nix/store/dddd-nixos-system-bastion"),
        true,
        Some(default_phase_metrics()),
        finished,
    )
    .await?;
    Ok(())
}

async fn seed_jobs_for_done_eval(
    store: &SqlxStore,
    eval_id: EvalId,
    started: DateTime<Utc>,
    finished: DateTime<Utc>,
) -> anyhow::Result<()> {
    finish_job(
        store,
        eval_id,
        "packages.x86_64-linux.argunix",
        "x86_64-linux",
        JobStatus::Success,
        Some("/nix/store/eeee-argunix-0.1.0"),
        true,
        Some(default_phase_metrics()),
        finished,
    )
    .await?;
    finish_job(
        store,
        eval_id,
        "packages.x86_64-linux.argunixctl",
        "x86_64-linux",
        JobStatus::Success,
        Some("/nix/store/ffff-argunixctl-0.1.0"),
        true,
        Some(default_phase_metrics()),
        started + chrono::Duration::minutes(3),
    )
    .await?;
    finish_job(
        store,
        eval_id,
        "checks.x86_64-linux.integration",
        "x86_64-linux",
        JobStatus::Failure,
        None,
        true,
        Some(default_phase_metrics()),
        finished,
    )
    .await?;
    finish_job(
        store,
        eval_id,
        "packages.aarch64-linux.argunix",
        "aarch64-linux",
        JobStatus::Cached,
        Some("/nix/store/gggg-argunix-0.1.0"),
        false,
        None,
        started + chrono::Duration::seconds(5),
    )
    .await?;
    finish_job(
        store,
        eval_id,
        "packages.aarch64-darwin.argunix",
        "aarch64-darwin",
        JobStatus::SkippedNoBuilder,
        None,
        false,
        None,
        finished,
    )
    .await?;
    Ok(())
}

async fn seed_jobs_for_in_flight_eval(
    store: &SqlxStore,
    eval_id: EvalId,
    building_started: DateTime<Utc>,
) -> anyhow::Result<()> {
    // Three running jobs dispatched to alpha (id=1) so the status page
    // running-rows table has body. Phases set to push/build/pull so
    // every phase badge renders.
    let alpha = BuilderId::new(1);
    let beta = BuilderId::new(2);
    let job_a = create_job(
        store,
        eval_id,
        "packages.x86_64-linux.argunix",
        "x86_64-linux",
        Some("/nix/store/p-argunix.drv"),
    )
    .await?;
    <SqlxStore as JobStore>::dispatch(store, job_a, alpha, building_started).await?;

    let job_b = create_job(
        store,
        eval_id,
        "packages.x86_64-linux.argunixctl",
        "x86_64-linux",
        Some("/nix/store/p-argunixctl.drv"),
    )
    .await?;
    <SqlxStore as JobStore>::dispatch(
        store,
        job_b,
        beta,
        building_started + chrono::Duration::seconds(10),
    )
    .await?;

    let job_c = create_job(
        store,
        eval_id,
        "checks.x86_64-linux.integration",
        "x86_64-linux",
        Some("/nix/store/p-int.drv"),
    )
    .await?;
    <SqlxStore as JobStore>::dispatch(
        store,
        job_c,
        alpha,
        building_started + chrono::Duration::seconds(20),
    )
    .await?;

    // Two queued jobs for the same eval — status page's "upcoming" table.
    create_job(
        store,
        eval_id,
        "packages.aarch64-linux.argunix",
        "aarch64-linux",
        Some("/nix/store/p-argunix-arm.drv"),
    )
    .await?;
    create_job(
        store,
        eval_id,
        "checks.aarch64-linux.integration",
        "aarch64-linux",
        Some("/nix/store/p-int-arm.drv"),
    )
    .await?;
    Ok(())
}

/// Seed a done eval with container-image jobs and their post-build
/// effects — a single-arch `oci` push and a multi-arch `docker` index
/// — so the eval page's "Published images" section, the job effects
/// panel, the image-size row, and the SBOM browser all have data.
async fn seed_image_jobs(
    store: &SqlxStore,
    eval_id: EvalId,
    started: DateTime<Utc>,
    finished: DateTime<Utc>,
) -> anyhow::Result<()> {
    let pushed = started + chrono::Duration::minutes(6);

    // A single-arch `oci` image: one job, pushed whole to `ghcr`, with
    // a CycloneDX SBOM attached as a referrer.
    let api = finish_job(
        store,
        eval_id,
        "packages.x86_64-linux.widget-api",
        "x86_64-linux",
        JobStatus::Success,
        Some("/nix/store/h1h1-widget-api-image.tar"),
        true,
        Some(default_phase_metrics()),
        finished,
    )
    .await?;
    <SqlxStore as JobStore>::record_image_size(store, api, 41_287_544).await?;
    effect_run(
        store,
        api,
        "registry-push",
        "ghcr",
        pushed,
        finished,
        "pushed ghcr.io/acme/widget-api:main, ghcr.io/acme/widget-api:latest, \
         ghcr.io/acme/widget-api:sha-c90000000000 to ghcr",
    )
    .await?;
    effect_run(
        store,
        api,
        "sbom-attach",
        "ghcr",
        pushed,
        finished,
        "attached SBOM (5 components) to ghcr.io/acme/widget-api:sha-c90000000000",
    )
    .await?;
    <SqlxStore as SbomStore>::upsert_sbom(store, api, "cyclonedx", FIXTURE_SBOM_JSON, 5, finished)
        .await?;

    // A multi-arch `docker` image: two per-arch jobs assembled into one
    // OCI index — each per-arch job carries a `registry-index` row.
    for (attr, system) in [
        ("packages.x86_64-linux.widget-worker", "x86_64-linux"),
        ("packages.aarch64-linux.widget-worker", "aarch64-linux"),
    ] {
        let job = finish_job(
            store,
            eval_id,
            attr,
            system,
            JobStatus::Success,
            Some("/nix/store/h2h2-widget-worker-image.tar"),
            true,
            Some(default_phase_metrics()),
            finished,
        )
        .await?;
        <SqlxStore as JobStore>::record_image_size(store, job, 9_842_113).await?;
        effect_run(
            store,
            job,
            "registry-index",
            "ghcr",
            pushed,
            finished,
            "assembled 2-arch index (amd64+arm64, 2 per-arch SBOMs) → \
             ghcr.io/acme/widget-worker:main, latest, sha-c90000000000",
        )
        .await?;
    }
    Ok(())
}

/// Record one finished (`success`) post-build effect against a job.
async fn effect_run(
    store: &SqlxStore,
    job: JobId,
    kind: &str,
    target: &str,
    started: DateTime<Utc>,
    finished: DateTime<Utc>,
    detail: &str,
) -> anyhow::Result<()> {
    let id =
        <SqlxStore as EffectRunStore>::create_effect_run(store, job, kind, target, started).await?;
    <SqlxStore as EffectRunStore>::finish_effect_run(store, id, "success", Some(detail), finished)
        .await?;
    Ok(())
}

/// A small CycloneDX document for the dev fixture's SBOM browser —
/// same shape `argunix-effects::sbom` emits (components carry a
/// `nix:store_path` property).
const FIXTURE_SBOM_JSON: &str = r#"{
  "bomFormat": "CycloneDX",
  "specVersion": "1.5",
  "version": 1,
  "metadata": { "component": { "type": "container", "name": "widget-api" } },
  "components": [
    { "type": "library", "name": "glibc", "version": "2.40-66",
      "purl": "pkg:nix/glibc@2.40-66",
      "properties": [{ "name": "nix:store_path",
        "value": "/nix/store/3p1wq8m6xyk0r2hn4v7zd9c5jf8lb0sg-glibc-2.40-66" }] },
    { "type": "library", "name": "openssl", "version": "3.4.1",
      "purl": "pkg:nix/openssl@3.4.1",
      "properties": [{ "name": "nix:store_path",
        "value": "/nix/store/k7m2n9p4xq1wr8t3yv6zd0c5jf8lb0sg-openssl-3.4.1" }] },
    { "type": "library", "name": "zlib", "version": "1.3.1",
      "purl": "pkg:nix/zlib@1.3.1",
      "properties": [{ "name": "nix:store_path",
        "value": "/nix/store/9w2e4r6t8y0u1i3o5p7a9s1d3f5g7h9j-zlib-1.3.1" }] },
    { "type": "library", "name": "libxcrypt", "version": "4.4.38",
      "purl": "pkg:nix/libxcrypt@4.4.38",
      "properties": [{ "name": "nix:store_path",
        "value": "/nix/store/0ksa3i39aqkwdrh2q0s1svwymhc1w3dm-libxcrypt-4.4.38" }] },
    { "type": "application", "name": "busybox", "version": "1.37.0",
      "purl": "pkg:nix/busybox@1.37.0",
      "properties": [{ "name": "nix:store_path",
        "value": "/nix/store/2b4d6f8h0j2l4n6p8r0t2v4x6z8a0c2e-busybox-1.37.0" }] }
  ]
}"#;

// ---------- helpers ----------

async fn upsert_repo(
    store: &SqlxStore,
    forge: &str,
    slug_str: &str,
    name: Option<&str>,
    description: Option<&str>,
    web_url: Option<&str>,
) -> anyhow::Result<RepoId> {
    let slug = Slug::new(slug_str.to_string()).expect("valid slug");
    let id = <SqlxStore as RepoStore>::upsert(store, forge, &slug).await?;
    // Dev fixture: every seeded repo has `main` as its default branch
    // so the badge endpoint exercises the default-branch path.
    <SqlxStore as RepoStore>::set_metadata(store, id, name, description, web_url, Some("main"))
        .await?;
    Ok(id)
}

async fn create_eval(
    store: &SqlxStore,
    repo_id: RepoId,
    trigger: &str,
    git_ref: &str,
    sha: Sha,
    pr_number: Option<u32>,
) -> anyhow::Result<EvalId> {
    let id = <SqlxStore as EvalStore>::create(
        store,
        NewEvaluation {
            repo_id,
            trigger: trigger.into(),
            git_ref: git_ref.into(),
            sha,
            pr_number,
        },
    )
    .await?;
    Ok(id)
}

async fn create_job(
    store: &SqlxStore,
    eval_id: EvalId,
    attr: &str,
    system: &str,
    drv_path: Option<&str>,
) -> anyhow::Result<JobId> {
    let id = <SqlxStore as JobStore>::create(
        store,
        NewJob {
            eval_id,
            attr_path: AttrPath::new(attr.to_string()),
            drv_path: drv_path.map(|s| s.to_string()),
            system: system.into(),
            main_program: None,
            outputs: Default::default(),
        },
    )
    .await?;
    Ok(id)
}

async fn finish_job(
    store: &SqlxStore,
    eval_id: EvalId,
    attr: &str,
    system: &str,
    status: JobStatus,
    output_path: Option<&str>,
    has_log: bool,
    metrics: Option<JobPhaseMetrics>,
    finished_at: DateTime<Utc>,
) -> anyhow::Result<JobId> {
    let id = create_job(
        store,
        eval_id,
        attr,
        system,
        Some("/nix/store/zz-fixture.drv"),
    )
    .await?;
    <SqlxStore as JobStore>::start(store, id, finished_at - chrono::Duration::seconds(90)).await?;
    let log_path = if has_log {
        Some("/dev/null/fixture.log.zst")
    } else {
        None
    };
    let metrics = metrics.unwrap_or_default();
    <SqlxStore as JobStore>::finish(
        store,
        id,
        status,
        finished_at,
        log_path,
        output_path,
        &metrics,
    )
    .await?;
    Ok(id)
}

fn default_phase_metrics() -> JobPhaseMetrics {
    JobPhaseMetrics {
        push_bytes: Some(12_345_678),
        push_ms: Some(1_240),
        build_ms: Some(85_400),
        pull_bytes: Some(523_876_544),
        pull_ms: Some(7_120),
        cache_push_ms: Some(161_500),
    }
}

async fn enroll_builder(
    store: &SqlxStore,
    name: &str,
    pubkey: &[u8; 32],
    capabilities: BuilderCapabilities,
    last_seen: DateTime<Utc>,
) -> anyhow::Result<BuilderId> {
    let new = NewBuilder {
        name: BuilderName::new(name.to_string()).expect("valid builder name"),
        pubkey: BuilderPubkey::from_bytes(pubkey).expect("valid pubkey"),
        capabilities,
    };
    let id = <SqlxStore as BuilderStore>::upsert(store, new, last_seen).await?;
    Ok(id)
}

fn register_live(
    registry: &BuilderRegistry,
    name: &str,
    builder_id: BuilderId,
    state: ConnState,
    capabilities: BuilderCapabilities,
    connected_since: DateTime<Utc>,
) {
    let n = BuilderName::new(name.to_string()).expect("valid builder name");
    let conn = ConnectedBuilder {
        builder_id,
        capabilities,
        state,
        connected_since,
        connection_id: registry.next_connection_id(),
        // Dev mode never opens build channels — no russh handle needed.
        session: None,
    };
    registry.register(n, conn);
}

/// Pad a short hex prefix into the 40-char SHA the domain type requires.
fn sha(prefix: &str) -> Sha {
    let mut s = String::from(prefix);
    while s.len() < 40 {
        s.push('0');
    }
    Sha::new(s).expect("valid sha")
}
