use crate::records::{EvalRecord, ForgeStatusRecord, JobRecord, NewEvaluation, NewJob, RepoRecord};
use crate::traits::{EvalStore, ForgeStatusStore, JobStore, RepoStore, StoreError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use medusa_domain::{
    AttrPath, EvalId, EvalStatus, JobId, JobStatus, RepoId, Sha, ShaError, Slug, SlugError,
};
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};
use std::str::FromStr;

#[derive(Clone)]
pub struct SqlxStore {
    pool: SqlitePool,
}

impl SqlxStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

fn to_eval_status(s: &str) -> Result<EvalStatus, StoreError> {
    EvalStatus::from_str(s).map_err(|_| StoreError::InvalidStatus(s.to_string()))
}

fn to_job_status(s: &str) -> Result<JobStatus, StoreError> {
    JobStatus::from_str(s).map_err(|_| StoreError::InvalidStatus(s.to_string()))
}

fn to_sha(id: i64, s: String) -> Result<Sha, StoreError> {
    Sha::new(s).map_err(|e: ShaError| StoreError::InvalidSha { id, error: e })
}

fn to_slug(id: i64, s: String) -> Result<Slug, StoreError> {
    Slug::new(s).map_err(|e: SlugError| StoreError::InvalidSlug { id, error: e })
}

fn map_repo(row: &SqliteRow) -> Result<RepoRecord, StoreError> {
    let id: i64 = row.try_get("id")?;
    let forge: String = row.try_get("forge")?;
    let slug: String = row.try_get("slug")?;
    Ok(RepoRecord {
        id: RepoId::new(id),
        forge,
        slug: to_slug(id, slug)?,
    })
}

fn map_eval(row: &SqliteRow) -> Result<EvalRecord, StoreError> {
    let id: i64 = row.try_get("id")?;
    let repo_id: i64 = row.try_get("repo_id")?;
    let trigger: String = row.try_get("trigger")?;
    let git_ref: String = row.try_get("git_ref")?;
    let sha: String = row.try_get("sha")?;
    let started_at: Option<DateTime<Utc>> = row.try_get("started_at")?;
    let finished_at: Option<DateTime<Utc>> = row.try_get("finished_at")?;
    let status: String = row.try_get("status")?;
    Ok(EvalRecord {
        id: EvalId::new(id),
        repo_id: RepoId::new(repo_id),
        trigger,
        git_ref,
        sha: to_sha(id, sha)?,
        started_at,
        finished_at,
        status: to_eval_status(&status)?,
    })
}

fn map_job(row: &SqliteRow) -> Result<JobRecord, StoreError> {
    let id: i64 = row.try_get("id")?;
    let eval_id: i64 = row.try_get("eval_id")?;
    let attr_path: String = row.try_get("attr_path")?;
    let drv_path: Option<String> = row.try_get("drv_path")?;
    let system: String = row.try_get("system")?;
    let started_at: Option<DateTime<Utc>> = row.try_get("started_at")?;
    let finished_at: Option<DateTime<Utc>> = row.try_get("finished_at")?;
    let status: String = row.try_get("status")?;
    let log_path: Option<String> = row.try_get("log_path")?;
    let output_path: Option<String> = row.try_get("output_path")?;
    Ok(JobRecord {
        id: JobId::new(id),
        eval_id: EvalId::new(eval_id),
        attr_path: AttrPath::new(attr_path),
        drv_path,
        system,
        started_at,
        finished_at,
        status: to_job_status(&status)?,
        log_path,
        output_path,
    })
}

#[async_trait]
impl RepoStore for SqlxStore {
    async fn upsert(&self, forge: &str, slug: &Slug) -> Result<RepoId, StoreError> {
        // ON CONFLICT DO UPDATE makes RETURNING work for both insert and existing rows.
        let row = sqlx::query(
            "INSERT INTO repos (forge, slug) VALUES (?1, ?2)
             ON CONFLICT(forge, slug) DO UPDATE SET slug = excluded.slug
             RETURNING id",
        )
        .bind(forge)
        .bind(slug.as_str())
        .fetch_one(&self.pool)
        .await?;
        let id: i64 = row.try_get("id")?;
        Ok(RepoId::new(id))
    }

    async fn get(&self, id: RepoId) -> Result<Option<RepoRecord>, StoreError> {
        let row = sqlx::query("SELECT id, forge, slug FROM repos WHERE id = ?1")
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(map_repo).transpose()
    }

    async fn find(&self, forge: &str, slug: &Slug) -> Result<Option<RepoRecord>, StoreError> {
        let row = sqlx::query("SELECT id, forge, slug FROM repos WHERE forge = ?1 AND slug = ?2")
            .bind(forge)
            .bind(slug.as_str())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(map_repo).transpose()
    }

    async fn list(&self) -> Result<Vec<RepoRecord>, StoreError> {
        let rows = sqlx::query("SELECT id, forge, slug FROM repos ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(map_repo).collect()
    }
}

#[async_trait]
impl EvalStore for SqlxStore {
    async fn create(&self, new: NewEvaluation) -> Result<EvalId, StoreError> {
        let row = sqlx::query(
            "INSERT INTO evaluations (repo_id, trigger, git_ref, sha, status)
             VALUES (?1, ?2, ?3, ?4, ?5)
             RETURNING id",
        )
        .bind(new.repo_id.get())
        .bind(new.trigger)
        .bind(new.git_ref)
        .bind(new.sha.as_str())
        .bind(EvalStatus::Queued.as_str())
        .fetch_one(&self.pool)
        .await?;
        let id: i64 = row.try_get("id")?;
        Ok(EvalId::new(id))
    }

    async fn get(&self, id: EvalId) -> Result<Option<EvalRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT id, repo_id, trigger, git_ref, sha, started_at, finished_at, status
             FROM evaluations WHERE id = ?1",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(map_eval).transpose()
    }

    async fn set_status(&self, id: EvalId, status: EvalStatus) -> Result<(), StoreError> {
        sqlx::query("UPDATE evaluations SET status = ?1 WHERE id = ?2")
            .bind(status.as_str())
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn finish(
        &self,
        id: EvalId,
        status: EvalStatus,
        finished_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE evaluations SET status = ?1, finished_at = ?2 WHERE id = ?3")
            .bind(status.as_str())
            .bind(finished_at)
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_by_repo(
        &self,
        repo_id: RepoId,
        limit: u32,
    ) -> Result<Vec<EvalRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, repo_id, trigger, git_ref, sha, started_at, finished_at, status
             FROM evaluations
             WHERE repo_id = ?1
             ORDER BY id DESC
             LIMIT ?2",
        )
        .bind(repo_id.get())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(map_eval).collect()
    }
}

#[async_trait]
impl JobStore for SqlxStore {
    async fn create(&self, new: NewJob) -> Result<JobId, StoreError> {
        let row = sqlx::query(
            "INSERT INTO jobs (eval_id, attr_path, drv_path, system, status)
             VALUES (?1, ?2, ?3, ?4, ?5)
             RETURNING id",
        )
        .bind(new.eval_id.get())
        .bind(new.attr_path.as_str())
        .bind(new.drv_path)
        .bind(new.system)
        .bind(JobStatus::Queued.as_str())
        .fetch_one(&self.pool)
        .await?;
        let id: i64 = row.try_get("id")?;
        Ok(JobId::new(id))
    }

    async fn get(&self, id: JobId) -> Result<Option<JobRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT id, eval_id, attr_path, drv_path, system, started_at, finished_at,
                    status, log_path, output_path
             FROM jobs WHERE id = ?1",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(map_job).transpose()
    }

    async fn list_by_eval(&self, eval_id: EvalId) -> Result<Vec<JobRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, eval_id, attr_path, drv_path, system, started_at, finished_at,
                    status, log_path, output_path
             FROM jobs WHERE eval_id = ?1 ORDER BY id",
        )
        .bind(eval_id.get())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(map_job).collect()
    }

    async fn set_status(&self, id: JobId, status: JobStatus) -> Result<(), StoreError> {
        sqlx::query("UPDATE jobs SET status = ?1 WHERE id = ?2")
            .bind(status.as_str())
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn start(&self, id: JobId, started_at: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("UPDATE jobs SET status = ?1, started_at = ?2 WHERE id = ?3")
            .bind(JobStatus::Running.as_str())
            .bind(started_at)
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn finish(
        &self,
        id: JobId,
        status: JobStatus,
        finished_at: DateTime<Utc>,
        log_path: Option<&str>,
        output_path: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE jobs
             SET status = ?1, finished_at = ?2, log_path = ?3, output_path = ?4
             WHERE id = ?5",
        )
        .bind(status.as_str())
        .bind(finished_at)
        .bind(log_path)
        .bind(output_path)
        .bind(id.get())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_running_interrupted(&self) -> Result<u64, StoreError> {
        let r = sqlx::query("UPDATE jobs SET status = ?1 WHERE status = ?2")
            .bind(JobStatus::Interrupted.as_str())
            .bind(JobStatus::Running.as_str())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}

#[async_trait]
impl ForgeStatusStore for SqlxStore {
    async fn upsert(
        &self,
        eval_id: EvalId,
        kind: &str,
        handle: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO forge_status (eval_id, kind, handle, last_posted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(eval_id, kind) DO UPDATE SET
                handle = excluded.handle,
                last_posted_at = excluded.last_posted_at",
        )
        .bind(eval_id.get())
        .bind(kind)
        .bind(handle)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(
        &self,
        eval_id: EvalId,
        kind: &str,
    ) -> Result<Option<ForgeStatusRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT eval_id, kind, handle, last_posted_at
             FROM forge_status WHERE eval_id = ?1 AND kind = ?2",
        )
        .bind(eval_id.get())
        .bind(kind)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let eval_id: i64 = row.try_get("eval_id")?;
        let kind: String = row.try_get("kind")?;
        let handle: Option<String> = row.try_get("handle")?;
        let last_posted_at: DateTime<Utc> = row.try_get("last_posted_at")?;
        Ok(Some(ForgeStatusRecord {
            eval_id: EvalId::new(eval_id),
            kind,
            handle,
            last_posted_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::{NewEvaluation, NewJob};

    async fn store() -> SqlxStore {
        let pool = crate::open_in_memory().await.unwrap();
        SqlxStore::new(pool)
    }

    #[tokio::test]
    async fn repo_upsert_round_trip() {
        let s = store().await;
        let slug = Slug::new("a/b").unwrap();
        let id = <SqlxStore as RepoStore>::upsert(&s, "github", &slug)
            .await
            .unwrap();
        let again = <SqlxStore as RepoStore>::upsert(&s, "github", &slug)
            .await
            .unwrap();
        assert_eq!(id, again);
        let r = <SqlxStore as RepoStore>::get(&s, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.forge, "github");
        assert_eq!(r.slug, slug);
        let by_find = <SqlxStore as RepoStore>::find(&s, "github", &slug)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_find.id, id);
        assert_eq!(<SqlxStore as RepoStore>::list(&s).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn eval_lifecycle() {
        let s = store().await;
        let repo_id = <SqlxStore as RepoStore>::upsert(&s, "github", &Slug::new("a/b").unwrap())
            .await
            .unwrap();
        let eval_id = <SqlxStore as EvalStore>::create(
            &s,
            NewEvaluation {
                repo_id,
                trigger: "push".to_string(),
                git_ref: "refs/heads/main".to_string(),
                sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            },
        )
        .await
        .unwrap();
        let r = <SqlxStore as EvalStore>::get(&s, eval_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.status, EvalStatus::Queued);

        <SqlxStore as EvalStore>::set_status(&s, eval_id, EvalStatus::Evaluating)
            .await
            .unwrap();
        let r = <SqlxStore as EvalStore>::get(&s, eval_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.status, EvalStatus::Evaluating);

        let now = Utc::now();
        <SqlxStore as EvalStore>::finish(&s, eval_id, EvalStatus::Done, now)
            .await
            .unwrap();
        let r = <SqlxStore as EvalStore>::get(&s, eval_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.status, EvalStatus::Done);
        assert!(r.finished_at.is_some());
    }

    #[tokio::test]
    async fn job_lifecycle_and_interrupted_recovery() {
        let s = store().await;
        let repo_id = <SqlxStore as RepoStore>::upsert(&s, "github", &Slug::new("a/b").unwrap())
            .await
            .unwrap();
        let eval_id = <SqlxStore as EvalStore>::create(
            &s,
            NewEvaluation {
                repo_id,
                trigger: "push".to_string(),
                git_ref: "refs/heads/main".to_string(),
                sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            },
        )
        .await
        .unwrap();
        let job_id = <SqlxStore as JobStore>::create(
            &s,
            NewJob {
                eval_id,
                attr_path: AttrPath::new("packages.x86_64-linux.foo"),
                drv_path: Some("/nix/store/xxx-foo.drv".to_string()),
                system: "x86_64-linux".to_string(),
            },
        )
        .await
        .unwrap();
        <SqlxStore as JobStore>::set_status(&s, job_id, JobStatus::Running)
            .await
            .unwrap();
        let n = <SqlxStore as JobStore>::mark_running_interrupted(&s)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let j = <SqlxStore as JobStore>::get(&s, job_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(j.status, JobStatus::Interrupted);

        let listed = <SqlxStore as JobStore>::list_by_eval(&s, eval_id)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, job_id);
    }

    #[tokio::test]
    async fn forge_status_upsert() {
        let s = store().await;
        let repo_id = <SqlxStore as RepoStore>::upsert(&s, "github", &Slug::new("a/b").unwrap())
            .await
            .unwrap();
        let eval_id = <SqlxStore as EvalStore>::create(
            &s,
            NewEvaluation {
                repo_id,
                trigger: "push".to_string(),
                git_ref: "refs/heads/main".to_string(),
                sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            },
        )
        .await
        .unwrap();
        <SqlxStore as ForgeStatusStore>::upsert(&s, eval_id, "pending", Some("check-1"))
            .await
            .unwrap();
        let r = <SqlxStore as ForgeStatusStore>::get(&s, eval_id, "pending")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.kind, "pending");
        assert_eq!(r.handle.as_deref(), Some("check-1"));

        // Update — same key, new handle
        <SqlxStore as ForgeStatusStore>::upsert(&s, eval_id, "pending", Some("check-2"))
            .await
            .unwrap();
        let r = <SqlxStore as ForgeStatusStore>::get(&s, eval_id, "pending")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.handle.as_deref(), Some("check-2"));
    }
}
