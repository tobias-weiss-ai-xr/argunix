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
    BuilderRecord, EvalRecord, EvalWithRepo, ForgeStatusRecord, JobPhaseMetrics, JobRecord,
    JobWithContext, NewBuilder, NewEvaluation, NewJob, RepoRecord,
};
pub use sqlite::SqlxStore;
pub use traits::{
    BuilderStore, EvalStore, ForgeStatusStore, InterruptOutcome, JobStore, MAX_INTERRUPTIONS,
    RepoStore, StoreError,
};

use sqlx::sqlite::SqlitePool;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("opening database: {0}")]
    Connect(#[from] sqlx::Error),
    #[error("running migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Open a sqlite database at `path`, creating the file if absent, run
/// migrations, and return a pool. Used by the daemon.
pub async fn open_at(path: &Path) -> Result<SqlitePool, OpenError> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
        )
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

/// Open an in-memory sqlite database with migrations applied. For tests.
pub async fn open_in_memory() -> Result<SqlitePool, OpenError> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(":memory:")
                .create_if_missing(true),
        )
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
