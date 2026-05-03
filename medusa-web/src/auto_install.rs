//! Startup webhook auto-installation.
//!
//! For each configured repo, ensure the forge has a webhook pointing at
//! medusa with a matching secret. Idempotent — runs every boot, no-ops
//! when the forge already agrees with sqlite.
//!
//! Best-effort: a forge being unreachable doesn't block daemon startup.
//! We log warnings per repo and the rest of medusa keeps working;
//! webhooks that didn't get installed will return
//! `WebhookNotProvisioned` (HTTP 503) when (if) they fire.

use medusa_config::{Config, Repo};
use medusa_domain::ForgeKind;
use medusa_forge::Provider;
use medusa_store::{RepoStore, SqlxStore};
use rand::RngCore;
use std::collections::HashMap;
use std::sync::Arc;

/// Walk every repo in `config`, ensure a webhook is installed and the
/// secret is recorded in sqlite. Logs at INFO on success, WARN on
/// failure; never errors out the whole pass.
pub async fn ensure_all(
    config: &Config,
    providers: &HashMap<String, Arc<dyn Provider>>,
    store: &SqlxStore,
) {
    let mut installed = 0usize;
    let mut failed = 0usize;
    for repo in &config.repos {
        match ensure_one(config, providers, store, repo).await {
            Ok(()) => installed += 1,
            Err(e) => {
                tracing::warn!(
                    forge = %repo.forge,
                    slug = %repo.slug,
                    error = %e,
                    "webhook auto-install failed; webhook events for this repo will be rejected with 503 until the next successful pass",
                );
                failed += 1;
            }
        }
    }
    tracing::info!(
        installed,
        failed,
        total = config.repos.len(),
        "webhook auto-install pass complete",
    );
}

async fn ensure_one(
    config: &Config,
    providers: &HashMap<String, Arc<dyn Provider>>,
    store: &SqlxStore,
    repo: &Repo,
) -> anyhow::Result<()> {
    let forge_cfg = config
        .forges
        .get(&repo.forge)
        .ok_or_else(|| anyhow::anyhow!("forge `{}` referenced by repo missing", repo.forge))?;
    let provider = providers
        .get(&repo.forge)
        .ok_or_else(|| anyhow::anyhow!("provider for forge `{}` not built", repo.forge))?;

    let kind = match forge_cfg.kind {
        ForgeKind::Github => "github",
        ForgeKind::Gitlab => "gitlab",
        ForgeKind::Forgejo => "forgejo",
    };
    let target_url = format!(
        "{}/webhook/{}",
        config.external_url.trim_end_matches('/'),
        kind,
    );

    let repo_id = store.upsert(&repo.forge, &repo.slug).await?;
    // Reuse the secret across calls so a previously-installed hook
    // keeps validating after a daemon restart. Generate one only on
    // the first encounter.
    //
    // The secret is stored as the *bytes of an ASCII hex string* (not
    // raw random bytes). This matters because forges store the secret
    // as text and HMAC-sign with `<text>.as_bytes()` as the key — for
    // medusa's verify side to use the same key bytes, we must keep
    // the secret in its string-form throughout. The providers send
    // `std::str::from_utf8(secret)` directly as the string secret in
    // their API payload; no hex re-encoding.
    let secret = match store.get_webhook_secret(&repo.forge, &repo.slug).await? {
        Some(s) if !s.is_empty() => s,
        _ => {
            let mut raw = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut raw);
            hex::encode(raw).into_bytes()
        }
    };

    let hook_id = provider
        .ensure_webhook(&repo.slug, &target_url, &secret)
        .await?;

    store.set_webhook(repo_id, &secret, &hook_id.0).await?;
    tracing::info!(
        forge = %repo.forge,
        slug = %repo.slug,
        target_url = %target_url,
        hook_id = %hook_id.0,
        "webhook ensured",
    );
    Ok(())
}
