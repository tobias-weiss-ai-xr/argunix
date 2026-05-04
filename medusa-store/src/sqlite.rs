use crate::records::{
    BuilderRecord, EvalRecord, ForgeStatusRecord, JobRecord, NewBuilder, NewEvaluation, NewJob,
    RepoRecord,
};
use crate::traits::{BuilderStore, EvalStore, ForgeStatusStore, JobStore, RepoStore, StoreError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use medusa_domain::{
    AttrPath, BuilderCapabilities, BuilderId, BuilderName, BuilderNameError, BuilderPubkey,
    BuilderPubkeyError, EvalId, EvalStatus, JobId, JobStatus, RepoId, Sha, ShaError, Slug,
    SlugError,
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

fn map_builder(row: &SqliteRow) -> Result<BuilderRecord, StoreError> {
    let id: i64 = row.try_get("id")?;
    let name_s: String = row.try_get("name")?;
    let pubkey_blob: Vec<u8> = row.try_get("pubkey")?;
    let systems_s: String = row.try_get("systems")?;
    let features_s: String = row.try_get("features")?;
    let max_jobs: i64 = row.try_get("max_jobs")?;
    let nix_version: String = row.try_get("nix_version")?;
    let enrolled_at: DateTime<Utc> = row.try_get("enrolled_at")?;
    let last_seen: DateTime<Utc> = row.try_get("last_seen")?;
    let revoked_at: Option<DateTime<Utc>> = row.try_get("revoked_at")?;

    let name = BuilderName::new(name_s)
        .map_err(|e: BuilderNameError| StoreError::InvalidBuilderName { id, error: e })?;
    let pubkey = BuilderPubkey::from_bytes(&pubkey_blob)
        .map_err(|e: BuilderPubkeyError| StoreError::InvalidBuilderPubkey { id, error: e })?;
    let systems: Vec<String> = serde_json::from_str(&systems_s)
        .map_err(|e| StoreError::InvalidBuilderCapabilities { id, error: e })?;
    let features: Vec<String> = serde_json::from_str(&features_s)
        .map_err(|e| StoreError::InvalidBuilderCapabilities { id, error: e })?;
    Ok(BuilderRecord {
        id: BuilderId::new(id),
        name,
        pubkey,
        capabilities: BuilderCapabilities {
            systems,
            features,
            max_jobs: max_jobs.max(0) as u32,
            nix_version,
        },
        enrolled_at,
        last_seen,
        revoked_at,
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

    async fn get_webhook_secret(
        &self,
        forge: &str,
        slug: &Slug,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let row = sqlx::query("SELECT webhook_secret FROM repos WHERE forge = ?1 AND slug = ?2")
            .bind(forge)
            .bind(slug.as_str())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Ok(None),
            Some(r) => {
                let blob: Option<Vec<u8>> = r.try_get("webhook_secret")?;
                Ok(blob)
            }
        }
    }

    async fn set_webhook(
        &self,
        repo_id: RepoId,
        secret: &[u8],
        hook_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE repos SET webhook_secret = ?1, webhook_hook_id = ?2 WHERE id = ?3")
            .bind(secret)
            .bind(hook_id)
            .bind(repo_id.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn prune_repos_not_in(
        &self,
        keep: &[(String, Slug)],
    ) -> Result<Vec<RepoRecord>, StoreError> {
        let mut tx = self.pool.begin().await?;

        let all_rows = sqlx::query("SELECT id, forge, slug FROM repos")
            .fetch_all(&mut *tx)
            .await?;
        let all: Vec<RepoRecord> = all_rows.iter().map(map_repo).collect::<Result<_, _>>()?;
        let kept: std::collections::HashSet<(&str, &str)> =
            keep.iter().map(|(f, s)| (f.as_str(), s.as_str())).collect();
        let orphans: Vec<RepoRecord> = all
            .into_iter()
            .filter(|r| !kept.contains(&(r.forge.as_str(), r.slug.as_str())))
            .collect();

        // Order matters: queue → forge_status → jobs → evaluations →
        // repos. Each statement scopes by repo_id via subqueries; the
        // whole thing runs in one transaction so a failure mid-cascade
        // leaves no partial state.
        for r in &orphans {
            let id = r.id.get();
            sqlx::query(
                "DELETE FROM queue WHERE job_id IN (
                     SELECT j.id FROM jobs j
                     JOIN evaluations e ON j.eval_id = e.id
                     WHERE e.repo_id = ?1
                 )",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "DELETE FROM forge_status WHERE eval_id IN (
                     SELECT id FROM evaluations WHERE repo_id = ?1
                 )",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "DELETE FROM jobs WHERE eval_id IN (
                     SELECT id FROM evaluations WHERE repo_id = ?1
                 )",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM evaluations WHERE repo_id = ?1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM repos WHERE id = ?1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(orphans)
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

    async fn list_active_by_branch_key(
        &self,
        repo_id: RepoId,
        branch_key_prefix: &str,
    ) -> Result<Vec<EvalRecord>, StoreError> {
        // We match either an exact git_ref or a `<key>:<anything>` form
        // (PR refs like `refs/pull/42/head:feature-x` need to match key
        // `refs/pull/42/head`). Plain prefix match with `LIKE` works:
        // we look for `<key>` exactly OR `<key>:%`.
        let like_pattern = format!("{}:%", branch_key_prefix.replace('\\', "\\\\"));
        let rows = sqlx::query(
            "SELECT id, repo_id, trigger, git_ref, sha, started_at, finished_at, status
             FROM evaluations
             WHERE repo_id = ?1
               AND status IN ('queued', 'evaluating', 'building')
               AND (git_ref = ?2 OR git_ref LIKE ?3 ESCAPE '\\')
             ORDER BY id ASC",
        )
        .bind(repo_id.get())
        .bind(branch_key_prefix)
        .bind(&like_pattern)
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
impl BuilderStore for SqlxStore {
    async fn upsert(&self, new: NewBuilder, now: DateTime<Utc>) -> Result<BuilderId, StoreError> {
        let systems_json =
            serde_json::to_string(&new.capabilities.systems).expect("Vec<String> serialises");
        let features_json =
            serde_json::to_string(&new.capabilities.features).expect("Vec<String> serialises");
        // ON CONFLICT(name): the row is the latest snapshot, so we overwrite
        // pubkey + capabilities + last_seen and clear `revoked_at`. Operators
        // who want to lock a builder out should `medusactl builders revoke`
        // *and* not redistribute the enrollment token; a builder showing up
        // with the right token after revocation is treated as a fresh enroll.
        let row = sqlx::query(
            "INSERT INTO builders
                 (name, pubkey, systems, features, max_jobs, nix_version,
                  enrolled_at, last_seen, revoked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, NULL)
             ON CONFLICT(name) DO UPDATE SET
                 pubkey       = excluded.pubkey,
                 systems      = excluded.systems,
                 features     = excluded.features,
                 max_jobs     = excluded.max_jobs,
                 nix_version  = excluded.nix_version,
                 last_seen    = excluded.last_seen,
                 revoked_at   = NULL
             RETURNING id",
        )
        .bind(new.name.as_str())
        .bind(new.pubkey.as_bytes().as_slice())
        .bind(systems_json)
        .bind(features_json)
        .bind(new.capabilities.max_jobs as i64)
        .bind(new.capabilities.nix_version)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        let id: i64 = row.try_get("id")?;
        Ok(BuilderId::new(id))
    }

    async fn get(&self, id: BuilderId) -> Result<Option<BuilderRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT id, name, pubkey, systems, features, max_jobs, nix_version,
                    enrolled_at, last_seen, revoked_at
             FROM builders WHERE id = ?1",
        )
        .bind(id.get())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(map_builder).transpose()
    }

    async fn find_by_name(&self, name: &str) -> Result<Option<BuilderRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT id, name, pubkey, systems, features, max_jobs, nix_version,
                    enrolled_at, last_seen, revoked_at
             FROM builders WHERE name = ?1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(map_builder).transpose()
    }

    async fn find_active_by_pubkey(
        &self,
        pubkey: &BuilderPubkey,
    ) -> Result<Option<BuilderRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT id, name, pubkey, systems, features, max_jobs, nix_version,
                    enrolled_at, last_seen, revoked_at
             FROM builders WHERE pubkey = ?1 AND revoked_at IS NULL",
        )
        .bind(pubkey.as_bytes().as_slice())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(map_builder).transpose()
    }

    async fn mark_seen(&self, id: BuilderId, now: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query("UPDATE builders SET last_seen = ?1 WHERE id = ?2")
            .bind(now)
            .bind(id.get())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn revoke(&self, name: &str, now: DateTime<Utc>) -> Result<bool, StoreError> {
        let r = sqlx::query("UPDATE builders SET revoked_at = ?1 WHERE name = ?2")
            .bind(now)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    async fn rename(&self, old: &str, new: &str) -> Result<bool, StoreError> {
        // Two-step under a transaction so we can distinguish
        // "old missing" from "new collides" cleanly.
        let mut tx = self.pool.begin().await?;
        let exists_new: Option<i64> = sqlx::query_scalar("SELECT id FROM builders WHERE name = ?1")
            .bind(new)
            .fetch_optional(&mut *tx)
            .await?;
        if exists_new.is_some() {
            return Ok(false);
        }
        let r = sqlx::query("UPDATE builders SET name = ?1 WHERE name = ?2")
            .bind(new)
            .bind(old)
            .execute(&mut *tx)
            .await?;
        let renamed = r.rows_affected() > 0;
        tx.commit().await?;
        Ok(renamed)
    }

    async fn list_all(&self) -> Result<Vec<BuilderRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, name, pubkey, systems, features, max_jobs, nix_version,
                    enrolled_at, last_seen, revoked_at
             FROM builders ORDER BY enrolled_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(map_builder).collect()
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

    #[tokio::test]
    async fn prune_drops_orphan_repos_with_cascade() {
        let s = store().await;
        let kept_slug = Slug::new("alice/keep").unwrap();
        let drop_slug = Slug::new("bob/drop").unwrap();

        // Set up two repos. Both get an evaluation and a job each so
        // we can verify the cascade really nukes the dependent rows.
        let kept_id = <SqlxStore as RepoStore>::upsert(&s, "github", &kept_slug)
            .await
            .unwrap();
        let drop_id = <SqlxStore as RepoStore>::upsert(&s, "old-forge", &drop_slug)
            .await
            .unwrap();

        let kept_eval = <SqlxStore as EvalStore>::create(
            &s,
            NewEvaluation {
                repo_id: kept_id,
                trigger: "push".into(),
                git_ref: "refs/heads/main".into(),
                sha: medusa_domain::Sha::new("0".repeat(40)).unwrap(),
            },
        )
        .await
        .unwrap();
        let drop_eval = <SqlxStore as EvalStore>::create(
            &s,
            NewEvaluation {
                repo_id: drop_id,
                trigger: "push".into(),
                git_ref: "refs/heads/main".into(),
                sha: medusa_domain::Sha::new("1".repeat(40)).unwrap(),
            },
        )
        .await
        .unwrap();

        let _ = <SqlxStore as JobStore>::create(
            &s,
            NewJob {
                eval_id: kept_eval,
                attr_path: AttrPath::new("packages.x86_64-linux.kept"),
                drv_path: None,
                system: "x86_64-linux".into(),
            },
        )
        .await
        .unwrap();
        let _ = <SqlxStore as JobStore>::create(
            &s,
            NewJob {
                eval_id: drop_eval,
                attr_path: AttrPath::new("packages.x86_64-linux.drop"),
                drv_path: None,
                system: "x86_64-linux".into(),
            },
        )
        .await
        .unwrap();

        // Prune with a `keep` list containing only the first repo.
        let pruned = <SqlxStore as RepoStore>::prune_repos_not_in(
            &s,
            &[("github".to_string(), kept_slug.clone())],
        )
        .await
        .unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].id, drop_id);

        // Repo gone.
        assert!(
            <SqlxStore as RepoStore>::get(&s, drop_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            <SqlxStore as RepoStore>::get(&s, kept_id)
                .await
                .unwrap()
                .is_some()
        );
        // Eval and job rows for the dropped repo gone too.
        assert!(
            <SqlxStore as EvalStore>::get(&s, drop_eval)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            <SqlxStore as JobStore>::list_by_eval(&s, drop_eval)
                .await
                .unwrap()
                .len(),
            0
        );
        // Kept repo's data untouched.
        assert!(
            <SqlxStore as EvalStore>::get(&s, kept_eval)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            <SqlxStore as JobStore>::list_by_eval(&s, kept_eval)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn prune_with_empty_keep_drops_everything() {
        let s = store().await;
        let _ = <SqlxStore as RepoStore>::upsert(&s, "gh", &Slug::new("a/b").unwrap())
            .await
            .unwrap();
        let _ = <SqlxStore as RepoStore>::upsert(&s, "gl", &Slug::new("c/d").unwrap())
            .await
            .unwrap();
        let pruned = <SqlxStore as RepoStore>::prune_repos_not_in(&s, &[])
            .await
            .unwrap();
        assert_eq!(pruned.len(), 2);
        assert_eq!(<SqlxStore as RepoStore>::list(&s).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn prune_no_op_when_all_repos_in_keep() {
        let s = store().await;
        let slug = Slug::new("a/b").unwrap();
        let _ = <SqlxStore as RepoStore>::upsert(&s, "gh", &slug)
            .await
            .unwrap();
        let pruned = <SqlxStore as RepoStore>::prune_repos_not_in(&s, &[("gh".to_string(), slug)])
            .await
            .unwrap();
        assert_eq!(pruned.len(), 0);
        assert_eq!(<SqlxStore as RepoStore>::list(&s).await.unwrap().len(), 1);
    }

    fn caps(systems: &[&str], features: &[&str], max_jobs: u32) -> BuilderCapabilities {
        BuilderCapabilities {
            systems: systems.iter().map(|s| s.to_string()).collect(),
            features: features.iter().map(|s| s.to_string()).collect(),
            max_jobs,
            nix_version: "2.18.1".into(),
        }
    }

    fn pubkey(seed: u8) -> BuilderPubkey {
        BuilderPubkey::from_bytes(&[seed; 32]).unwrap()
    }

    #[tokio::test]
    async fn builder_first_enrollment_round_trip() {
        let s = store().await;
        let now = Utc::now();
        let id = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("bobs-mini").unwrap(),
                pubkey: pubkey(1),
                capabilities: caps(&["aarch64-darwin"], &["big-parallel"], 2),
            },
            now,
        )
        .await
        .unwrap();

        let r = <SqlxStore as BuilderStore>::get(&s, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.name.as_str(), "bobs-mini");
        assert_eq!(r.pubkey, pubkey(1));
        assert_eq!(r.capabilities.systems, vec!["aarch64-darwin".to_string()]);
        assert_eq!(r.capabilities.max_jobs, 2);
        assert!(r.revoked_at.is_none());
        assert_eq!(r.enrolled_at, r.last_seen);
    }

    #[tokio::test]
    async fn builder_reconnect_overwrites_capabilities_and_clears_revocation() {
        let s = store().await;
        let t0 = Utc::now();
        let id = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("mac01").unwrap(),
                pubkey: pubkey(1),
                capabilities: caps(&["aarch64-darwin"], &[], 1),
            },
            t0,
        )
        .await
        .unwrap();
        // Operator revokes…
        assert!(
            <SqlxStore as BuilderStore>::revoke(&s, "mac01", t0)
                .await
                .unwrap()
        );
        let r = <SqlxStore as BuilderStore>::get(&s, id)
            .await
            .unwrap()
            .unwrap();
        assert!(r.revoked_at.is_some());

        // …then re-enrolls with a new key + updated caps. Same name → upsert
        // overwrites and clears `revoked_at`.
        let t1 = t0 + chrono::Duration::seconds(60);
        let id2 = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("mac01").unwrap(),
                pubkey: pubkey(2),
                capabilities: caps(&["aarch64-darwin", "x86_64-darwin"], &["kvm"], 4),
            },
            t1,
        )
        .await
        .unwrap();
        assert_eq!(id, id2);
        let r = <SqlxStore as BuilderStore>::get(&s, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.pubkey, pubkey(2));
        assert_eq!(r.capabilities.systems.len(), 2);
        assert_eq!(r.capabilities.features, vec!["kvm".to_string()]);
        assert_eq!(r.capabilities.max_jobs, 4);
        assert!(r.revoked_at.is_none());
        assert_eq!(r.last_seen, t1);
    }

    #[tokio::test]
    async fn pubkey_lookup_skips_revoked() {
        let s = store().await;
        let now = Utc::now();
        let _ = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("a").unwrap(),
                pubkey: pubkey(7),
                capabilities: caps(&["x86_64-linux"], &[], 1),
            },
            now,
        )
        .await
        .unwrap();
        assert!(
            <SqlxStore as BuilderStore>::find_active_by_pubkey(&s, &pubkey(7))
                .await
                .unwrap()
                .is_some()
        );
        <SqlxStore as BuilderStore>::revoke(&s, "a", now)
            .await
            .unwrap();
        // Revoked → pubkey auth must fail; agent then retries with the
        // enrollment token (see design/builders.md auth state machine).
        assert!(
            <SqlxStore as BuilderStore>::find_active_by_pubkey(&s, &pubkey(7))
                .await
                .unwrap()
                .is_none()
        );
        // But find_by_name still returns it so `medusactl builders` can
        // show revoked rows.
        assert!(
            <SqlxStore as BuilderStore>::find_by_name(&s, "a")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn mark_seen_advances_last_seen_only() {
        let s = store().await;
        let t0 = Utc::now();
        let id = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("a").unwrap(),
                pubkey: pubkey(1),
                capabilities: caps(&["x86_64-linux"], &[], 1),
            },
            t0,
        )
        .await
        .unwrap();
        let t1 = t0 + chrono::Duration::seconds(120);
        <SqlxStore as BuilderStore>::mark_seen(&s, id, t1)
            .await
            .unwrap();
        let r = <SqlxStore as BuilderStore>::get(&s, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.enrolled_at, t0);
        assert_eq!(r.last_seen, t1);
    }

    #[tokio::test]
    async fn revoke_unknown_returns_false() {
        let s = store().await;
        assert!(
            !<SqlxStore as BuilderStore>::revoke(&s, "nope", Utc::now())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn rename_happy_and_collision() {
        let s = store().await;
        let now = Utc::now();
        let _ = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("old").unwrap(),
                pubkey: pubkey(1),
                capabilities: caps(&["x86_64-linux"], &[], 1),
            },
            now,
        )
        .await
        .unwrap();
        // Happy path
        assert!(
            <SqlxStore as BuilderStore>::rename(&s, "old", "new")
                .await
                .unwrap()
        );
        assert!(
            <SqlxStore as BuilderStore>::find_by_name(&s, "new")
                .await
                .unwrap()
                .is_some()
        );
        // Old gone
        assert!(
            <SqlxStore as BuilderStore>::find_by_name(&s, "old")
                .await
                .unwrap()
                .is_none()
        );
        // Set up a collision target and try to rename onto it
        let _ = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("other").unwrap(),
                pubkey: pubkey(2),
                capabilities: caps(&["x86_64-linux"], &[], 1),
            },
            now,
        )
        .await
        .unwrap();
        assert!(
            !<SqlxStore as BuilderStore>::rename(&s, "new", "other")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn list_all_includes_revoked_oldest_first() {
        let s = store().await;
        let t0 = Utc::now();
        let _ = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("first").unwrap(),
                pubkey: pubkey(1),
                capabilities: caps(&["x86_64-linux"], &[], 1),
            },
            t0,
        )
        .await
        .unwrap();
        let _ = <SqlxStore as BuilderStore>::upsert(
            &s,
            NewBuilder {
                name: BuilderName::new("second").unwrap(),
                pubkey: pubkey(2),
                capabilities: caps(&["aarch64-linux"], &[], 1),
            },
            t0 + chrono::Duration::seconds(10),
        )
        .await
        .unwrap();
        <SqlxStore as BuilderStore>::revoke(&s, "first", t0 + chrono::Duration::seconds(20))
            .await
            .unwrap();
        let all = <SqlxStore as BuilderStore>::list_all(&s).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name.as_str(), "first");
        assert!(all[0].revoked_at.is_some());
        assert_eq!(all[1].name.as_str(), "second");
        assert!(all[1].revoked_at.is_none());
    }
}
