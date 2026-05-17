//! Persistence layer.
//!
//! Two things in scope here:
//! - traits the rest of argunix talks to (`RepoStore`, `EvalStore`, `JobStore`,
//!   `ForgeStatusStore`),
//! - a concrete `SqlxStore` impl backed by sqlx-sqlite (postgres comes later).
//!
//! Migrations live in `argunix-store/migrations/` and are embedded at compile
//! time via `sqlx::migrate!`.

mod records;
mod sqlite;
mod traits;

pub use records::{
    BuilderRecord, DockerImageRecord, EffectRunRecord, EvalJobTally, EvalRecord, EvalWithRepo,
    ForgeStatusRecord, JobPhaseMetrics, JobRecord, JobWithContext, NewBuilder, NewDockerImage,
    NewEvaluation, NewJob, RepoRecord,
};
pub use sqlite::SqlxStore;
pub use traits::{
    BuilderStore, DockerImageStore, EffectRunStore, EvalStore, ForgeStatusStore, InterruptOutcome,
    JobStore, MAX_INTERRUPTIONS, RepoStore, StoreError,
};

use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("opening database: {0}")]
    Connect(#[from] sqlx::Error),
    #[error("running migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// How long a connection waits on a locked database before giving up
/// with `SQLITE_BUSY` ("database is locked"). SQLite serializes
/// writers; with WAL, a second writer simply waits this long for the
/// first to commit. 30s is far longer than any argunix write takes —
/// if it ever fires, something is genuinely wedged. The wait is clean
/// (no deadlock) because no store method holds a read lock it then
/// tries to upgrade — see [`file_pool_options`].
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Pool for the file-backed store.
///
/// **Multi-connection.** WAL lets readers run concurrently with the
/// single writer, so the daemon's many readers — the web UI, the
/// registry, the worker's status lookups — never queue behind a
/// write. (An earlier single-connection design serialized *everything*
/// onto one connection; under a build-storm that turned every read
/// into a wait behind the slowest write. sqlx's own "slow to acquire
/// connection" warnings flagged it.)
///
/// A multi-connection pool is only safe because no store method does
/// `SELECT`-then-write inside a transaction: that pattern holds a
/// shared lock and then asks to upgrade, and two of them at once
/// *deadlock* — SQLite breaks the tie with an immediate `SQLITE_BUSY`
/// that `busy_timeout` cannot absorb. argunix's read-modify-write
/// operations (`JobStore::record_interruption`,
/// `BuilderStore::rename`) are therefore each a single
/// `UPDATE` statement. Plain writer-vs-writer contention is the only
/// thing left, and `busy_timeout` covers it.
fn file_pool_options() -> SqlitePoolOptions {
    SqlitePoolOptions::new()
        .max_connections(8)
        .min_connections(1)
}

/// Pool for the in-memory store (tests, dev server).
///
/// `:memory:` is *per-connection* — each connection is its own empty
/// database — so there can be exactly one connection, pinned for the
/// pool's lifetime. Recycling it (`max_lifetime` / `idle_timeout`)
/// would discard the whole database mid-test.
fn memory_pool_options() -> SqlitePoolOptions {
    SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .max_lifetime(None)
        .idle_timeout(None)
}

/// Connection options for the file-backed store. Every pragma argunix
/// relies on lives here, on [`SqliteConnectOptions`], so sqlx applies
/// it to every connection it opens — the sqlite footgun (a `PRAGMA`
/// set on one connection does not carry to the next) is handled once.
///
/// - **WAL**: readers never block the writer; survives across the
///   daemon / CLI process boundary.
/// - **`synchronous = FULL`**: a committed transaction survives a
///   power loss or hard reset, not merely a clean application crash.
///   argunix's crash-recovery redrive depends on this — on restart the
///   daemon resumes interrupted evaluations from their *persisted*
///   status, so an eval that reached `building` before a hard crash
///   must still read back as `building`. `NORMAL` is faster but lets
///   WAL commits between checkpoints roll back on power loss — exactly
///   the eval-state loss that would strand a resumed build.
/// - **`busy_timeout`**: see [`BUSY_TIMEOUT`].
/// - **`foreign_keys = ON`**: SQLite leaves foreign-key *enforcement*
///   off by default, per connection. argunix's schema declares every
///   relationship with `ON DELETE CASCADE`, and retention GC / repo
///   pruning delete only the top row and rely on the cascade — so
///   enforcement must be on, on every connection.
fn file_connect_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(BUSY_TIMEOUT)
        .foreign_keys(true)
}

/// Open a sqlite database at `path`, creating the file if absent, run
/// migrations, and return a pool. Used by the daemon and the
/// single-shot `argunix build` CLI.
pub async fn open_at(path: &Path) -> Result<SqlitePool, OpenError> {
    let pool = file_pool_options()
        .connect_with(file_connect_options(path))
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Open an in-memory sqlite database with migrations applied. For tests
/// and the `argunix-web` dev server.
///
/// WAL / `synchronous` are meaningless for `:memory:` (no file, no
/// journal), so only [`memory_pool_options`] matters here. Foreign-key
/// enforcement is still set so tests exercise the same
/// `ON DELETE CASCADE` behaviour the daemon runs.
pub async fn open_in_memory() -> Result<SqlitePool, OpenError> {
    let pool = memory_pool_options()
        .connect_with(
            SqliteConnectOptions::new()
                .filename(":memory:")
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EvalStore, NewEvaluation, RepoStore, SqlxStore};
    use argunix_domain::{Sha, Slug};

    /// Many tasks writing at once must all succeed — no `database is
    /// locked`. Exercises the real file-backed pool + WAL config, the
    /// path the daemon actually runs.
    #[tokio::test]
    async fn concurrent_writes_never_lock() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_at(&dir.path().join("db.sqlite")).await.unwrap();
        let store = SqlxStore::new(pool);

        let repo_id =
            <SqlxStore as RepoStore>::upsert(&store, "github", &Slug::new("a/b").unwrap())
                .await
                .unwrap();

        // 64 concurrent evaluation inserts. Without a sane pool /
        // pragma setup this is exactly where `database is locked`
        // surfaces.
        let mut set = tokio::task::JoinSet::new();
        for i in 0..64u32 {
            let store = store.clone();
            set.spawn(async move {
                <SqlxStore as EvalStore>::create(
                    &store,
                    NewEvaluation {
                        repo_id,
                        trigger: "push".into(),
                        git_ref: format!("refs/heads/b{i}"),
                        sha: Sha::new("0".repeat(40)).unwrap(),
                        pr_number: None,
                    },
                )
                .await
            });
        }
        while let Some(joined) = set.join_next().await {
            joined
                .expect("insert task panicked")
                .expect("concurrent insert must not hit `database is locked`");
        }
    }
}
