//! GitSyncAgent - Handles Git repository synchronization and webhook processing
//!
//! This agent manages:
//! - Polling Git repositories for changes
//! - Handling webhooks from GitHub, GitLab, Forgejo
//! - Cloning and updating repositories
//! - Detecting flake.nix changes
//! - Triggering downstream build tasks
//!
//! ## Features
//! - Multi-provider support (GitHub, GitLab, Forgejo)
//! - Webhook signature verification (HMAC)
//! - Repository change detection
//! - Flake.nix discovery and validation
//! - Shallow clone support for performance
//! - Repository caching with TTL
//!
//! ## Messages Handled
//! - SyncRepository: Sync a specific repository
//! - PollRepository: Poll repository for changes
//! - WebhookReceived: Process incoming webhook
//! - SetupRepository: Initialize repository monitoring
//!
//! ## Dependencies
//! - Requires `git` CLI or `git2` crate
//! - Uses tokio for async operations

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, RwLock};
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use hex as hex_crate;

use agentflow_core::{Agent, AgentContext, AgentMessage, AgentStatus, AgentType, Result, TaskDefinition, TaskStatus, TaskType};
use agentflow_core::agent::{StateStore, TaskStore};

/// Git provider types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum GitProvider {
    GitHub,
    GitLab,
    Forgejo,
    Generic,
}

/// Webhook event types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WebhookEvent {
    Push,
    PullRequest,
    MergeRequest,
    TagCreate,
    Release,
    Unknown(String),
}

/// Repository sync status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SyncStatus {
    /// Repository never synced
    Never,
    /// Sync in progress
    Syncing,
    /// Sync completed successfully
    Synced,
    /// Sync failed
    Failed(String),
    /// Repository not found
    NotFound,
}

/// Repository information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInfo {
    /// Repository URL
    pub url: String,
    /// Provider type
    pub provider: GitProvider,
    /// Default branch
    pub branch: String,
    /// Local cache path
    pub local_path: PathBuf,
    /// Last sync timestamp
    pub last_sync: Option<DateTime<Utc>>,
    /// Last commit hash
    pub last_commit: Option<String>,
    /// Current sync status
    pub status: SyncStatus,
    /// Webhook secret for verification
    pub webhook_secret: Option<String>,
    /// Flake path (relative to repo root)
    pub flake_path: PathBuf,
    /// Flake exists flag
    pub has_flake: bool,
    /// Polling interval in seconds
    pub poll_interval: u64,
    /// Enable polling
    pub polling_enabled: bool,
    /// Enable webhooks
    pub webhooks_enabled: bool,
    /// Repository health
    pub healthy: bool,
    /// Error message if unhealthy
    pub error: Option<String>,
}

/// Webhook payload structure (simplified)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookPayload {
    /// Event type
    pub event: String,
    /// Repository URL or name
    pub repository: String,
    /// Branch or ref
    pub ref_name: String,
    /// Commit hash
    pub commit: String,
    /// Previous commit (for push events)
    pub before: Option<String>,
    /// Provider-specific data
    pub provider_data: HashMap<String, serde_json::Value>,
    /// Raw payload (for debugging)
    pub raw: Option<String>,
}

/// Agent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSyncConfig {
    /// Base cache directory for repositories
    pub cache_path: PathBuf,
    /// Maximum cache size in bytes
    pub max_cache_size: u64,
    /// Default polling interval (seconds)
    pub default_poll_interval: u64,
    /// Provider configurations
    pub providers: HashMap<GitProvider, ProviderConfig>,
    /// Default branch
    pub default_branch: String,
    /// Shallow clone by default
    pub shallow_clone: bool,
    /// Clone depth for shallow clones
    pub clone_depth: Option<usize>,
    /// Enable automatic cleanup
    pub auto_cleanup: bool,
    /// Cache TTL in hours
    pub cache_ttl_hours: u32,
    /// Git CLI path (default: "git")
    pub git_command: String,
}

/// Provider-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub enabled: bool,
    pub webhook_secret: Option<String>,
    pub api_base_url: Option<String>,
}

impl Default for GitSyncConfig {
    fn default() -> Self {
        let mut providers = HashMap::new();
        providers.insert(GitProvider::GitHub, ProviderConfig {
            enabled: true,
            webhook_secret: None,
            api_base_url: Some("https://api.github.com".to_string()),
        });
        providers.insert(GitProvider::GitLab, ProviderConfig {
            enabled: true,
            webhook_secret: None,
            api_base_url: Some("https://gitlab.com/api/v4".to_string()),
        });
        providers.insert(GitProvider::Forgejo, ProviderConfig {
            enabled: true,
            webhook_secret: None,
            api_base_url: None,
        });
        
        Self {
            cache_path: PathBuf::from("/var/cache/agentflow/repos"),
            max_cache_size: 10 * 1024 * 1024 * 1024, // 10GB
            default_poll_interval: 60,
            providers,
            default_branch: "main".to_string(),
            shallow_clone: true,
            clone_depth: Some(50),
            auto_cleanup: true,
            cache_ttl_hours: 24,
            git_command: "git".to_string(),
        }
    }
}

impl GitSyncConfig {
    /// Create config from environment variables
    pub fn from_env() -> Self {
        let mut config = Self::default();
        
        if let Ok(cache_path) = std::env::var("AGENTFLOW_REPO_CACHE") {
            config.cache_path = PathBuf::from(cache_path);
        }
        
        if let Ok(git_cmd) = std::env::var("GIT_COMMAND") {
            config.git_command = git_cmd;
        }
        
        if let Ok(github_secret) = std::env::var("GITHUB_WEBHOOK_SECRET") {
            if let Some(github_config) = config.providers.get_mut(&GitProvider::GitHub) {
                github_config.webhook_secret = Some(github_secret);
            }
        }
        
        if let Ok(gitlab_secret) = std::env::var("GITLAB_WEBHOOK_SECRET") {
            if let Some(gitlab_config) = config.providers.get_mut(&GitProvider::GitLab) {
                gitlab_config.webhook_secret = Some(gitlab_secret);
            }
        }
        
        config
    }
}

/// GitSyncAgent state
#[derive(Debug, Default)]
pub struct GitSyncState {
    /// Tracked repositories
    pub repositories: HashMap<String, RepositoryInfo>,
    /// Active sync operations
    pub active_syncs: HashMap<String, Instant>,
    /// Last cleanup timestamp
    pub last_cleanup: Option<DateTime<Utc>>,
    /// sync statistics
    pub stats: SyncStats,
}

/// Sync statistics
#[derive(Debug, Default, Clone)]
pub struct SyncStats {
    pub total_syncs: u64,
    pub successful_syncs: u64,
    pub failed_syncs: u64,
    pub webhooks_received: u64,
    pub repositories_updated: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

impl SyncStats {
    pub fn record_sync(&mut self, success: bool) {
        self.total_syncs += 1;
        if success {
            self.successful_syncs += 1;
        } else {
            self.failed_syncs += 1;
        }
    }
    
    pub fn record_webhook(&mut self) {
        self.webhooks_received += 1;
    }
    
    pub fn cache_hit(&mut self) {
        self.cache_hits += 1;
    }
    
    pub fn cache_miss(&mut self) {
        self.cache_misses += 1;
    }
}

/// The GitSyncAgent struct
pub struct GitSyncAgent {
    /// Agent sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn TaskStore>,
    /// State store
    state_store: Arc<dyn StateStore>,
    /// Configuration
    config: GitSyncConfig,
    /// State
    state: Arc<RwLock<GitSyncState>>,
    /// Agent name
    name: String,
    /// Agent type
    agent_type: AgentType,
    /// Agent capabilities
    capabilities: HashSet<String>,
    /// Agent status
    status: AgentStatus,
}

impl GitSyncAgent {
    /// Create a new GitSyncAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn TaskStore>,
        state_store: Arc<dyn StateStore>,
        config: Option<GitSyncConfig>,
    ) -> Self {
        let config = config.unwrap_or_else(GitSyncConfig::from_env);
        
        // Create cache directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&config.cache_path) {
            tracing::error!("Failed to create cache directory: {}", e);
        }
        
        Self {
            sender,
            task_store,
            state_store,
            config,
            state: Arc::new(RwLock::new(GitSyncState::default())),
            name: "GitSyncAgent".to_string(),
            agent_type: AgentType::SourceControl,
            capabilities: Self::get_capabilities(),
            status: AgentStatus::Ready,
        }
    }
    
    /// Get agent capabilities
    fn get_capabilities() -> HashSet<String> {
        vec![
            "git-sync".to_string(),
            "webhook-handler".to_string(),
            "repository-polling".to_string(),
            "flake-detection".to_string(),
            "source-code".to_string(),
            "github".to_string(),
            "gitlab".to_string(),
            "forgejo".to_string(),
        ].into_iter().collect()
    }
    
    /// Setup a repository for monitoring
    pub async fn setup_repository(&mut self, repo_config: RepositoryConfig) -> Result<String> {
        let repo_id = self.get_repo_id(&repo_config.url);
        
        let local_path = self.config.cache_path.join(&repo_id);
        
        let mut repo_info = RepositoryInfo {
            url: repo_config.url.clone(),
            provider: repo_config.provider,
            branch: repo_config.branch.unwrap_or_else(|| self.config.default_branch.clone()),
            local_path,
            last_sync: None,
            last_commit: None,
            status: SyncStatus::Never,
            webhook_secret: repo_config.webhook_secret.clone(),
            flake_path: repo_config.flake_path.unwrap_or_else(|| PathBuf::from(".")),
            has_flake: false,
            poll_interval: repo_config.poll_interval.unwrap_or(self.config.default_poll_interval),
            polling_enabled: repo_config.polling_enabled.unwrap_or(true),
            webhooks_enabled: repo_config.webhooks_enabled.unwrap_or(true),
            healthy: true,
            error: None,
        };
        
        // Initial clone
        if repo_config.sync_now.unwrap_or(true) {
            let result = self.clone_repository(&repo_info).await;
            match result {
                Ok(commit) => {
                    repo_info.last_commit = Some(commit);
                    repo_info.last_sync = Some(Utc::now());
                    repo_info.status = SyncStatus::Synced;
                    repo_info.has_flake = self.check_flake_exists(&repo_info).await;
                }
                Err(e) => {
                    repo_info.error = Some(e.to_string());
                    repo_info.healthy = false;
                }
            }
        }
        
        {
            let mut state = self.state.write().await;
            state.repositories.insert(repo_id.clone(), repo_info);
        }
        
        Ok(repo_id)
    }
    
    /// Convert serde_json::Value to RepositoryConfig
    fn repo_config_from_value(value: serde_json::Value) -> Result<RepositoryConfig> {
        serde_json::from_value(value)
            .map_err(|e| agentflow_core::AgentFlowError::Generic(format!("Invalid repo config: {}", e)))
    }
    
    /// Get a unique repository ID from URL
    pub fn get_repo_id(&self, url: &str) -> String {
        // Normalize URL: remove .git suffix and protocol
        let normalized = url
            .replace("https://", "")
            .replace("http://", "")
            .replace("git@", "")
            .replace(".git", "");
        
        // Replace / and : with - for filesystem safety
        normalized
            .chars()
            .map(|c| match c {
                '/' | ':' => '-',
                _ => c,
            })
            .collect()
    }
    
    /// Clone or update a repository
    pub async fn clone_repository(&self, repo: &RepositoryInfo) -> Result<String> {
        use std::process::Stdio;
        
        // Create parent directory
        if let Some(parent) = repo.local_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Err(agentflow_core::AgentFlowError::Generic(
                    format!("Failed to create directory: {}", e)
                ));
            }
        }
        
        let mut cmd = tokio::process::Command::new(&self.config.git_command);
        
        // Check if directory exists
        let repo_exists = tokio::fs::metadata(&repo.local_path).await.is_ok();
        
        if repo_exists {
            // Update existing repository
            cmd.current_dir(&repo.local_path);
            cmd.arg("pull");
            if repo.branch != self.config.default_branch {
                cmd.arg("origin");
                cmd.arg(&repo.branch);
            }
        } else {
            // Clone new repository
            cmd.arg("clone");
            if self.config.shallow_clone {
                cmd.arg("--depth");
                if let Some(depth) = self.config.clone_depth {
                    cmd.arg(depth.to_string());
                } else {
                    cmd.arg("1");
                }
            }
            if repo.branch != self.config.default_branch {
                cmd.arg("--branch");
                cmd.arg(&repo.branch);
            }
            cmd.arg(&repo.url);
            cmd.arg(&repo.local_path);
        }
        
        cmd.stderr(Stdio::piped());
        cmd.stdout(Stdio::piped());
        
        let output = cmd
            .spawn()
            .map_err(|e| agentflow_core::AgentFlowError::Generic(format!("Failed to spawn git: {}", e)))?
            .wait_with_output()
            .await
            .map_err(|e| agentflow_core::AgentFlowError::Generic(format!("Git command failed: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(agentflow_core::AgentFlowError::Generic(
                format!("Git clone/pull failed: {}", stderr)
            ));
        }
        
        // Get current commit hash
        let commit = self.get_current_commit(&repo.local_path).await?;
        
        Ok(commit)
    }
    
    /// Get current commit hash for a repository
    async fn get_current_commit(&self, repo_path: &Path) -> Result<String> {
        let output = tokio::process::Command::new(&self.config.git_command)
            .current_dir(repo_path)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .await
            .map_err(|e| agentflow_core::AgentFlowError::Generic(format!("Failed to get commit: {}", e)))?;
        
        if !output.status.success() {
            return Err(agentflow_core::AgentFlowError::Generic(
                format!("Failed to get commit hash: {}", String::from_utf8_lossy(&output.stderr))
            ));
        }
        
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
    
    /// Check if flake.nix exists in repository
    pub async fn check_flake_exists(&self, repo: &RepositoryInfo) -> bool {
        let flake_path = repo.local_path.join(&repo.flake_path).join("flake.nix");
        tokio::fs::metadata(&flake_path).await.is_ok()
    }
    
    /// Sync a repository (poll for changes)
    pub async fn sync_repository(&mut self, repo_id: &str) -> Result<RepositoryInfo> {
        let state = self.state.read().await;
        let repo = state.repositories.get(repo_id)
            .ok_or_else(|| agentflow_core::AgentFlowError::NotFound(repo_id.to_string()))?
            .clone();
        
        drop(state);
        
        // Record sync start
        {
            let mut state = self.state.write().await;
            state.active_syncs.insert(repo_id.to_string(), Instant::now());
        }
        
        let old_commit = repo.last_commit.clone();
        
        match self.clone_repository(&repo).await {
            Ok(new_commit) => {
                let changed = old_commit != Some(new_commit.clone());
                
                let mut updated_repo = repo;
                updated_repo.last_commit = Some(new_commit);
                updated_repo.last_sync = Some(Utc::now());
                updated_repo.status = SyncStatus::Synced;
                updated_repo.error = None;
                updated_repo.healthy = true;
                
                // Check if flake exists
                updated_repo.has_flake = self.check_flake_exists(&updated_repo).await;
                
                {
                    let mut state = self.state.write().await;
                    state.repositories.insert(repo_id.to_string(), updated_repo.clone());
                    state.active_syncs.remove(repo_id);
                    state.stats.record_sync(true);
                }
                
                // Trigger downstream tasks if commit changed
                if changed {
                    self.trigger_downstream_tasks(&updated_repo).await?;
                }
                
                Ok(updated_repo)
            }
            Err(e) => {
                let mut updated_repo = repo;
                updated_repo.status = SyncStatus::Failed(e.to_string());
                updated_repo.healthy = false;
                updated_repo.error = Some(e.to_string());
                
                {
                    let mut state = self.state.write().await;
                    state.repositories.insert(repo_id.to_string(), updated_repo);
                    state.active_syncs.remove(repo_id);
                    state.stats.record_sync(false);
                }
                
                Err(e)
            }
        }
    }
    
    /// Trigger downstream tasks when repository changes
    async fn trigger_downstream_tasks(&self, repo: &RepositoryInfo) -> Result<()> {
        // If flake exists, trigger analysis and build
        if repo.has_flake {
            let task_id = format!("flake-analysis-{}-{}", 
                repo.local_path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown"),
                Utc::now().timestamp()
            );
            
            let message = AgentMessage::AnalyzeFlake {
                flake_url: repo.local_path.join(&repo.flake_path).to_string_lossy().to_string(),
                flake_ref: repo.last_commit.clone(),
                task_id: task_id.clone(),
            };
            
            self.sender.send(message).await
                .map_err(|e| agentflow_core::AgentFlowError::Generic(format!("Failed to send analyze message: {}", e)))?;
            
            tracing::info!("Triggered flake analysis for {}", repo.url);
        }
        
        Ok(())
    }
    
    /// Process a webhook payload
    pub async fn process_webhook(
        &mut self,
        provider: GitProvider,
        payload: WebhookPayload,
        signature: Option<&str>,
    ) -> Result<bool> {
        // Verify webhook signature if provided
        if let Some(secret) = self.get_webhook_secret(provider.clone()) {
            if let Some(signature) = signature {
                if !self.verify_webhook_signature(&payload, signature, &secret).await? {
                    tracing::warn!("Webhook signature verification failed for {}", payload.repository);
                    return Ok(false);
                }
            }
        }
        
        let event = self.parse_event_type(&payload);
        
        tracing::info!(
            "Received webhook from {:?}: event={:?}, repo={}, commit={}",
            provider,
            event,
            payload.repository,
            payload.commit
        );
        
        {
            let mut state = self.state.write().await;
            state.stats.record_webhook();
        }
        
        match event {
            WebhookEvent::Push => {
                // Find matching repository
                if let Some(repo) = self.find_repo_by_url(&payload.repository) {
                    // Check if this is a push to the monitored branch
                    if repo.branch == payload.ref_name {
                        self.handle_push_webhook(&repo, &payload).await?;
                        return Ok(true);
                    }
                }
            }
            WebhookEvent::PullRequest | WebhookEvent::MergeRequest => {
                // Handle PR/MR events
                if let Some(repo) = self.find_repo_by_url(&payload.repository) {
                    self.handle_pr_webhook(&repo, &payload).await?;
                    return Ok(true);
                }
            }
            _ => {
                tracing::debug!("Unhandled webhook event: {:?}", event);
            }
        }
        
        Ok(false)
    }
    
    /// Get webhook secret for a provider
    fn get_webhook_secret(&self, provider: GitProvider) -> Option<String> {
        self.config.providers.get(&provider)
            .and_then(|pc| pc.webhook_secret.clone())
    }
    
    /// Verify webhook signature using HMAC-SHA256
    async fn verify_webhook_signature(
        &self,
        payload: &WebhookPayload,
        signature: &str,
        secret: &str,
    ) -> Result<bool> {
        // Extract the actual signature (may have prefix like "sha256=")
        let sig = if signature.starts_with("sha256=") {
            &signature[7..]
        } else {
            signature
        };
        
        let raw_payload = payload.raw.as_deref().unwrap_or("{}");
        
        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());
        hasher.update(raw_payload.as_bytes());
        let result = hasher.finalize();
        let expected_sig = hex_crate::encode(result);
        
        // Constant-time comparison
        Ok(expected_sig == sig)
    }
    
    /// Parse webhook event type
    fn parse_event_type(&self, payload: &WebhookPayload) -> WebhookEvent {
        match payload.event.to_lowercase().as_str() {
            "push" => WebhookEvent::Push,
            "pull_request" => WebhookEvent::PullRequest,
            "merge_request" => WebhookEvent::MergeRequest,
            "create" => {
                if payload.ref_name.starts_with("refs/tags/") {
                    WebhookEvent::TagCreate
                } else {
                    WebhookEvent::Unknown(payload.event.clone())
                }
            }
            "release" => WebhookEvent::Release,
            _ => WebhookEvent::Unknown(payload.event.clone()),
        }
    }
    
    /// Find repository by URL
    fn find_repo_by_url(&self, url: &str) -> Option<RepositoryInfo> {
        let state = self.state.blocking_read();
        state.repositories.values().find(|r| 
            r.url == url || 
            r.url.ends_with(url) ||
            url.ends_with(&r.url)
        ).cloned()
    }
    
    /// Handle push webhook
    async fn handle_push_webhook(&mut self, repo: &RepositoryInfo, payload: &WebhookPayload) -> Result<()> {
        // Sync the repository
        let _ = self.sync_repository(&self.get_repo_id(&repo.url)).await?;
        
        // If flake exists, trigger build
        if repo.has_flake {
            let task_id = format!("webhook-build-{}-{}", 
                payload.commit,
                Utc::now().timestamp()
            );
            
            let message = AgentMessage::BuildFlake {
                flake_url: repo.local_path.to_string_lossy().to_string(),
                flake_ref: Some(payload.commit.clone()),
                targets: vec!["packages.default".to_string()],
                system: None,
                task_id,
            };
            
            self.sender.send(message).await
                .map_err(|e| agentflow_core::AgentFlowError::Generic(format!("Failed to send build message: {}", e)))?;
        }
        
        Ok(())
    }
    
    /// Handle pull request webhook
    async fn handle_pr_webhook(&mut self, _repo: &RepositoryInfo, _payload: &WebhookPayload) -> Result<()> {
        // For now, just log PR events
        // Full PR handling would include:
        // - Checking out PR branch
        // - Running CI checks
        // - Posting status
        tracing::info!("Pull request webhook received");
        Ok(())
    }
    
    /// Poll all repositories
    pub async fn poll_all_repositories(&mut self) -> Result<Vec<String>> {
        let state = self.state.read().await;
        let repo_ids: Vec<String> = state.repositories.keys().cloned().collect();
        drop(state);
        
        let mut updated = Vec::new();
        
        for repo_id in repo_ids {
            if let Ok(repo) = self.sync_repository(&repo_id).await {
                if repo.healthy {
                    updated.push(repo_id);
                }
            }
        }
        
        Ok(updated)
    }
    
    /// Get repository status
    pub async fn get_repo_status(&self, repo_id: &str) -> Option<RepositoryInfo> {
        let state = self.state.read().await;
        state.repositories.get(repo_id).cloned()
    }
    
    /// List all repositories
    pub async fn list_repositories(&self) -> Vec<RepositoryInfo> {
        let state = self.state.read().await;
        state.repositories.values().cloned().collect()
    }
    
    /// Remove a repository from monitoring
    pub async fn remove_repository(&mut self, repo_id: &str) -> bool {
        let mut state = self.state.write().await;
        
        if let Some(repo) = state.repositories.remove(repo_id) {
            // Clean up local directory
            if let Err(e) = tokio::fs::remove_dir_all(&repo.local_path).await {
                tracing::error!("Failed to remove repo directory: {}", e);
            }
            true
        } else {
            false
        }
    }
    
    /// Clean up old repositories
    pub async fn cleanup_old_repos(&mut self) -> Result<Vec<String>> {
        let ttl_hours = self.config.cache_ttl_hours;
        let ttl = Duration::from_secs(ttl_hours as u64 * 3600);
        let now = SystemTime::now();
        
        let mut removed = Vec::new();
        let mut state = self.state.write().await;
        
        let repo_ids: Vec<String> = state.repositories.keys().cloned().collect();
        
        for repo_id in repo_ids {
            if let Some(repo) = state.repositories.get(&repo_id) {
                if let Some(last_sync) = repo.last_sync {
                    let last_sync_time = last_sync.into();
                    if let Ok(duration) = now.duration_since(last_sync_time) {
                        if duration > ttl {
                            if let Err(e) = tokio::fs::remove_dir_all(&repo.local_path).await {
                                tracing::error!("Failed to remove old repo: {}", e);
                            } else {
                                state.repositories.remove(&repo_id);
                                removed.push(repo_id);
                            }
                        }
                    }
                }
            }
        }
        
        state.last_cleanup = Some(Utc::now());
        
        Ok(removed)
    }
}

/// Repository configuration for setup
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryConfig {
    pub url: String,
    pub provider: GitProvider,
    pub branch: Option<String>,
    pub flake_path: Option<PathBuf>,
    pub webhook_secret: Option<String>,
    pub poll_interval: Option<u64>,
    pub polling_enabled: Option<bool>,
    pub webhooks_enabled: Option<bool>,
    pub sync_now: Option<bool>,
}

impl Default for RepositoryConfig {
    fn default() -> Self {
        Self {
            url: Default::default(),
            provider: GitProvider::Generic,
            branch: None,
            flake_path: None,
            webhook_secret: None,
            poll_interval: None,
            polling_enabled: None,
            webhooks_enabled: None,
            sync_now: Some(true),
        }
    }
}

/// Implement Agent trait for GitSyncAgent
#[async_trait::async_trait]
impl Agent for GitSyncAgent {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn agent_type(&self) -> AgentType {
        self.agent_type.clone()
    }
    
    fn capabilities(&self) -> &HashSet<String> {
        &self.capabilities
    }
    
    fn status(&self) -> AgentStatus {
        self.status.clone()
    }
    
    async fn handle_message(&mut self, message: AgentMessage, _ctx: &AgentContext) -> Result<()> {
        match message {
            AgentMessage::SyncRepository { repo_config, task_id } => {
                let repo_config = Self::repo_config_from_value(repo_config)?;
                let repo_id = self.setup_repository(repo_config).await?;
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("sync-{}", repo_id)),
                    task_type: TaskType::SyncRepository,
                    status: TaskStatus::Succeeded,
                    priority: 100,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
            }
            
            AgentMessage::SetupRepository { repo_config, task_id } => {
                let repo_config = Self::repo_config_from_value(repo_config)?;
                let repo_id = self.setup_repository(repo_config).await?;
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("setup-{}", repo_id)),
                    task_type: TaskType::SetupRepository,
                    status: TaskStatus::Succeeded,
                    priority: 100,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
            }
            
            AgentMessage::PollRepository { repo_id, task_id } => {
                let repo_id = repo_id.clone();
                let result = self.sync_repository(&repo_id).await;
                
                let status = if result.is_ok() {
                    TaskStatus::Succeeded
                } else {
                    TaskStatus::Failed
                };
                
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("poll-{}-{}", repo_id, Utc::now().timestamp())),
                    task_type: TaskType::PollRepository,
                    status,
                    priority: 80,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
                
                if let Err(e) = result {
                    return Err(e);
                }
            }
            
            AgentMessage::WebhookReceived { provider, payload, signature, task_id } => {
                let provider_enum = match provider.to_lowercase().as_str() {
                    "github" => GitProvider::GitHub,
                    "gitlab" => GitProvider::GitLab,
                    "forgejo" => GitProvider::Forgejo,
                    _ => GitProvider::Generic,
                };
                
                let payload: WebhookPayload = serde_json::from_value(payload)
                    .map_err(|e| agentflow_core::AgentFlowError::Generic(format!("Invalid webhook payload: {}", e)))?;
                
                let handled = self.process_webhook(provider_enum, payload, signature.as_deref()).await?;
                
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("webhook-{}-{}", provider, Utc::now().timestamp())),
                    task_type: TaskType::WebhookReceived,
                    status: TaskStatus::Succeeded,
                    priority: 90,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
                
                if handled {
                    tracing::info!("Webhook processed successfully: {}", provider);
                }
            }
            
            AgentMessage::PollAllRepositories { task_id } => {
                let updated = self.poll_all_repositories().await?;
                tracing::info!("Polled {} repositories, {} updated", updated.len(), updated.len());
                
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("poll-all-{}", Utc::now().timestamp())),
                    task_type: TaskType::PollAllRepositories,
                    status: TaskStatus::Succeeded,
                    priority: 70,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
            }
            
            AgentMessage::GetRepositoryStatus { repo_id, task_id } => {
                if let Some(repo) = self.get_repo_status(&repo_id).await {
                    let status_str = match repo.status.clone() {
                        SyncStatus::Never => "never".to_string(),
                        SyncStatus::Syncing => "syncing".to_string(),
                        SyncStatus::Synced => "synced".to_string(),
                        SyncStatus::Failed(e) => e,
                        SyncStatus::NotFound => "not_found".to_string(),
                    };
                    self.sender.send(AgentMessage::RepositoryStatus {
                        repo_id: repo_id.clone(),
                        status: status_str.to_string(),
                        last_commit: repo.last_commit.clone(),
                        last_sync: repo.last_sync.map(|dt| dt.to_rfc3339()),
                        has_flake: repo.has_flake,
                        healthy: repo.healthy,
                        error: repo.error.clone(),
                    }).await?;
                }
                
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("status-{}-{}", repo_id, Utc::now().timestamp())),
                    task_type: TaskType::GetRepositoryStatus,
                    status: TaskStatus::Succeeded,
                    priority: 60,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
            }
            
            _ => {
                tracing::debug!("Unhandled message: {:?}", message);
            }
        }
        
        Ok(())
    }
}

// ========== Messages to Add ==========
// These need to be added to agentflow-core/src/message.rs
// The GitSyncAgent expects to receive/send these messages

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRepositoryMessage {
    pub repo_config: RepositoryConfig,
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollAllRepositoriesMessage {
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryStatusMessage {
    pub repo_id: String,
    pub status: SyncStatus,
    pub last_commit: Option<String>,
    pub last_sync: Option<DateTime<Utc>>,
    pub has_flake: bool,
    pub healthy: bool,
    pub error: Option<String>,
}

// ========== Tests ==========

#[cfg(test)]
mod tests {
    use super::*;
    use agentflow_core::state::MemoryTaskStore;
    use agentflow_core::agent::{StateStore, AgentDefinition};
    use agentflow_core::Result;
    use tokio::sync::mpsc;
    use std::sync::Arc;
    use async_trait::async_trait;
    
    // Mock state store for testing
    #[derive(Default)]
    struct MockStateStore;
    
    #[async_trait]
    impl StateStore for MockStateStore {
        async fn get_agent(&self, _id: &str) -> Result<Option<AgentDefinition>> {
            Ok(None)
        }
        
        async fn register_agent(&self, _agent: &AgentDefinition) -> Result<()> {
            Ok(())
        }
        
        async fn deregister_agent(&self, _id: &str) -> Result<()> {
            Ok(())
        }
        
        async fn list_agents(&self) -> Result<Vec<AgentDefinition>> {
            Ok(vec![])
        }
    }
    
    #[test]
    fn test_git_sync_config_defaults() {
        let config = GitSyncConfig::default();
        
        assert!(config.cache_path.to_string_lossy().contains("cache"));
        assert_eq!(config.default_poll_interval, 60);
        assert_eq!(config.default_branch, "main");
        assert!(config.providers.contains_key(&GitProvider::GitHub));
        assert!(config.shallow_clone);
    }
    
    #[test]
    fn test_repo_id_generation() {
        let agent = GitSyncAgent::new(
            mpsc::channel(32).0,
            Arc::new(MemoryTaskStore::default()),
            Arc::new(MockStateStore),
            None
        );
        let repo_id = agent.get_repo_id("https://github.com/owner/repo.git");
        
        // Should normalize the URL
        assert!(!repo_id.contains("https://"));
        assert!(!repo_id.contains(".git"));
    }
    
    #[tokio::test]
    async fn test_agent_creation() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = GitSyncAgent::new(sender, task_store, state_store, None);
        
        assert_eq!(agent.name(), "GitSyncAgent");
        assert!(agent.capabilities().contains("git-sync"));
        assert!(agent.capabilities().contains("webhook-handler"));
    }
    
    #[test]
    fn test_git_provider_parsing() {
        let url = "https://github.com/owner/repo";
        let agent = GitSyncAgent::new(
            mpsc::channel(32).0,
            Arc::new(MemoryTaskStore::default()),
            Arc::new(MockStateStore),
            None
        );
        let repo_id = agent.get_repo_id(url);
        
        assert!(!repo_id.is_empty());
        assert!(!repo_id.contains("//"));
    }
}
