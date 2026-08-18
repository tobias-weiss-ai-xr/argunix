//! GitHubStatusAgent - Post status updates to GitHub
//!
//! This agent handles:
//! - Posting commit status to GitHub repos
//! - Updating existing statuses
//! - Handling rate limits
//! - Various status states (pending, success, failure, error)
//!
//! ## Features
//! - GitHub API v3 support
//! - Personal Access Token authentication
//! - Rate limit tracking and handling
//! - Configurable descriptions for each status type
//!
//! ## Messages Handled
//! - PostGitHubStatus: Post a new status
//! - UpdateGitHubStatus: Update existing status
//! - NotifyGitHub: Send generic notification
//!
//! ## Environment Variables
//! - `GITHUB_TOKEN`: Required for authentication

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, RwLock};
use serde::{Deserialize, Serialize};
use async_trait::async_trait;

use agentflow_core::{Agent, AgentContext, AgentMessage, AgentStatus, AgentType, Result, TaskDefinition, TaskStatus, TaskType};
use agentflow_core::agent::{StateStore, TaskStore};

/// GitHub status state
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum GitHubStatusState {
    /// Status is pending
    Pending,
    /// Status is success
    Success,
    /// Status is failure
    Failure,
    /// Status is error
    Error,
}

impl std::fmt::Display for GitHubStatusState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitHubStatusState::Pending => write!(f, "pending"),
            GitHubStatusState::Success => write!(f, "success"),
            GitHubStatusState::Failure => write!(f, "failure"),
            GitHubStatusState::Error => write!(f, "error"),
        }
    }
}

/// GitHub repository info
#[derive(Debug, Clone)]
pub struct GitHubRepo {
    pub owner: String,
    pub repo: String,
}

/// GitHub commit
#[derive(Debug, Clone)]
pub struct GitHubCommit {
    pub repo: GitHubRepo,
    pub sha: String,
    pub ref_name: Option<String>,
}

/// Rate limit state from GitHub
#[derive(Debug, Default, Clone)]
pub struct RateLimitState {
    pub limit: u32,
    pub remaining: u32,
    pub reset_at: i64,
    pub last_request: Option<DateTime<Utc>>,
}

/// Agent statistics
#[derive(Debug, Default, Clone)]
pub struct GitHubStats {
    pub statuses_posted: u64,
    pub requests_made: u64,
    pub errors: u64,
    pub rate_limit_hits: u64,
    pub by_state: HashMap<GitHubStatusState, u64>,
}

/// Agent state
#[derive(Debug, Default, Clone)]
pub struct GitHubAgentStatus {
    /// Rate limit tracking
    pub rate_limit: RateLimitState,
    /// Statistics
    pub stats: GitHubStats,
    /// Cached repository info
    pub repos: HashMap<String, GitHubRepo>,
}

/// Description templates for each status type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusTemplates {
    #[serde(default = "default_pending_template")]
    pub pending: String,
    #[serde(default = "default_success_template")]
    pub success: String,
    #[serde(default = "default_failure_template")]
    pub failure: String,
    #[serde(default = "default_error_template")]
    pub error: String,
}

impl Default for StatusTemplates {
    fn default() -> Self {
        Self {
            pending: default_pending_template(),
            success: default_success_template(),
            failure: default_failure_template(),
            error: default_error_template(),
        }
    }
}

fn default_pending_template() -> String { "Nix build in progress...".to_string() }
fn default_success_template() -> String { "Nix build succeeded".to_string() }
fn default_failure_template() -> String { "Nix build failed".to_string() }
fn default_error_template() -> String { "Nix build error".to_string() }

/// Configuration for GitHubStatusAgent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubConfig {
    /// GitHub API URL
    #[serde(default = "default_api_url")]
    pub api_url: String,
    /// User agent string
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Custom status description templates
    #[serde(default)]
    pub description_templates: StatusTemplates,
    /// Use formatting
    #[serde(default)]
    pub use_formatting: bool,
}

fn default_api_url() -> String { "https://api.github.com".to_string() }
fn default_user_agent() -> String { "argunix-agentflow/0.1.0".to_string() }

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            api_url: default_api_url(),
            user_agent: default_user_agent(),
            description_templates: StatusTemplates::default(),
            use_formatting: true,
        }
    }
}

/// The GitHubStatusAgent
pub struct GitHubStatusAgent {
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn TaskStore>,
    /// State store
    _state_store: Arc<dyn StateStore>,
    /// Configuration
    config: GitHubConfig,
    /// State (shared across clones)
    state: Arc<RwLock<GitHubAgentStatus>>,
    /// HTTP client (using reqwest)
    client: reqwest::Client,
    /// GitHub token (from environment)
    token: Option<String>,
    /// Agent name
    name: String,
    /// Agent type
    agent_type: AgentType,
    /// Agent capabilities
    capabilities: HashSet<String>,
    /// Agent status
    status: AgentStatus,
}

impl GitHubStatusAgent {
    /// Create a new GitHubStatusAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn TaskStore>,
        state_store: Arc<dyn StateStore>,
        config: Option<GitHubConfig>,
    ) -> Self {
        // Get token from environment
        let token = std::env::var("GITHUB_TOKEN").ok();
        let has_token = token.as_ref().is_some();
        
        // Build HTTP client
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_str(&config.as_ref().unwrap_or(&GitHubConfig::default()).user_agent).unwrap(),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        
        let client_builder = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30));
        
        let client = if let Some(ref token_val) = token {
            let mut auth_headers = reqwest::header::HeaderMap::new();
            auth_headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token_val)).unwrap(),
            );
            client_builder.default_headers(auth_headers).build().unwrap()
        } else {
            client_builder.build().unwrap()
        };
        
        let capabilities = vec![
            "github-status".to_string(),
            "commit-status".to_string(),
            "pull-request-status".to_string(),
        ].into_iter().collect();
        
        Self {
            sender,
            task_store,
            _state_store: state_store,
            config: config.unwrap_or_default(),
            state: Arc::new(RwLock::new(GitHubAgentStatus::default())),
            client,
            token,
            name: "GitHubStatusAgent".to_string(),
            agent_type: AgentType::Custom,
            capabilities,
            status: if has_token { AgentStatus::Ready } else { AgentStatus::Error },
        }
    }
    
    /// Post status to GitHub
    pub async fn post_status(
        &mut self,
        owner: String,
        repo: String,
        sha: String,
        state: Option<GitHubStatusState>,
        description: Option<String>,
        target_url: Option<String>,
        context: Option<String>,
    ) -> Result<Option<String>> {
        let token = self.token.as_ref().ok_or_else(|| {
            agentflow_core::AgentFlowError::Generic("GITHUB_TOKEN not configured".to_string())
        })?;
        
        let state_str = match &state {
            Some(s) => s.to_string(),
            None => "pending".to_string(),
        };
        
        let description = description.or_else(|| {
            match &state {
                Some(GitHubStatusState::Pending) => Some(self.config.description_templates.pending.clone()),
                Some(GitHubStatusState::Success) => Some(self.config.description_templates.success.clone()),
                Some(GitHubStatusState::Failure) => Some(self.config.description_templates.failure.clone()),
                Some(GitHubStatusState::Error) => Some(self.config.description_templates.error.clone()),
                None => Some(self.config.description_templates.pending.clone()),
            }
        });
        
        let context = context.unwrap_or_else(|| "ci/nix-build".to_string());
        
        let url = format!(
            "{}/repos/{}/{}/statuses/{}",
            self.config.api_url, owner, repo, sha
        );
        
        let mut body = serde_json::json!({
            "state": state_str,
            "description": description,
            "context": context,
        });
        
        if let Some(tgt_url) = target_url {
            body["target_url"] = serde_json::Value::String(tgt_url);
        }
        
        let response = self._send_withRetry(url.clone(), "post".to_string(), Some(body), token).await?;
        
        if response.status().is_success() {
            let status_url = response.headers().get("Location").and_then(|h| h.to_str().ok()).map(String::from);
            
            {
                let mut agt_state = self.state.write().await;
                agt_state.stats.statuses_posted += 1;
                let status_key = state.clone().unwrap_or(GitHubStatusState::Pending);
                *agt_state.stats.by_state.entry(status_key).or_insert(0) += 1;
            }
            
            Ok(status_url)
        } else {
            let error_msg = format!("GitHub API error: {} - {}", 
                response.status(), 
                response.text().await.unwrap_or_default());
            Err(agentflow_core::AgentFlowError::Generic(error_msg))
        }
    }
    
    /// Send request with rate limit handling and retries
    async fn _send_withRetry(
        &self,
        url: String,
        method: String,
        body: Option<serde_json::Value>,
        token: &str,
    ) -> Result<reqwest::Response> {
        use std::time::Duration;
        
        let max_retries = 3;
        let mut retries = 0;
        
        loop {
            self.checkRateLimit().await?;
            
            let client = reqwest::Client::new();
            let mut req = match method.to_lowercase().as_str() {
                "post" => client.post(&url),
                "get" => client.get(&url),
                "put" => client.put(&url),
                "delete" => client.delete(&url),
                _ => client.post(&url),
            };
            
            req = req.bearer_auth(token);
            if let Some(ref b) = body {
                req = req.json(b);
            }
            
            let response = req.send().await;
            
            match response {
                Ok(resp) => {
                    self.updateRateLimit(resp.headers()).await;
                    
                    if resp.status() == reqwest::StatusCode::FORBIDDEN || 
                       resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        if retries < max_retries {
                            let retry_after = resp.headers().get("Retry-After")
                                .and_then(|h| h.to_str().ok())
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(0);
                            if retry_after == 0 {
                                let reset_at = {
                                    let state = self.state.read().await;
                                    state.rate_limit.reset_at
                                };
                                let now = Utc::now().timestamp();
                                let wait = std::cmp::max(0, reset_at - now);
                                tokio::time::sleep(Duration::from_secs(wait as u64)).await;
                            } else {
                                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                            }
                            retries += 1;
                            continue;
                        }
                    }
                    
                    return Ok(resp);
                }
                Err(e) => {
                    if retries < max_retries {
                        tokio::time::sleep(Duration::from_secs(2u64.pow(retries))).await;
                        retries += 1;
                        continue;
                    }
                    return Err(agentflow_core::AgentFlowError::Network(e.to_string()));
                }
            }
        }
    }
    
    /// Check rate limit before making request
    async fn checkRateLimit(&self) -> Result<()> {
        let state = self.state.read().await;
        if state.rate_limit.remaining == 0 {
            let reset_at = state.rate_limit.reset_at;
            let now = Utc::now().timestamp();
            if reset_at > now {
                let wait = std::cmp::max(1, reset_at - now);
                return Err(agentflow_core::AgentFlowError::Generic(
                    format!("Rate limit exceeded, waiting {} seconds", wait)
                ));
            }
        }
        Ok(())
    }
    
    /// Update rate limit from response headers
    async fn updateRateLimit(&self, headers: &reqwest::header::HeaderMap) {
        let mut state = self.state.write().await;
        
        if let Some(remaining) = headers.get("X-RateLimit-Remaining") {
            if let Ok(remaining_str) = remaining.to_str() {
                if let Ok(remaining_val) = remaining_str.parse::<u32>() {
                    state.rate_limit.remaining = remaining_val;
                }
            }
        }
        
        if let Some(limit) = headers.get("X-RateLimit-Limit") {
            if let Ok(limit_str) = limit.to_str() {
                if let Ok(limit_val) = limit_str.parse::<u32>() {
                    state.rate_limit.limit = limit_val;
                }
            }
        }
        
        if let Some(reset) = headers.get("X-RateLimit-Reset") {
            if let Ok(reset_str) = reset.to_str() {
                if let Ok(reset_val) = reset_str.parse::<i64>() {
                    state.rate_limit.reset_at = reset_val;
                }
            }
        }
        
        state.rate_limit.last_request = Some(Utc::now());
    }
}

/// Implement Agent trait for GitHubStatusAgent
#[async_trait::async_trait]
impl Agent for GitHubStatusAgent {
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
            AgentMessage::PostGitHubStatus { owner, repo, sha, state, description, target_url, task_id } => {
                let state_enum = state.map(|s| match s.to_lowercase().as_str() {
                    "pending" => GitHubStatusState::Pending,
                    "success" => GitHubStatusState::Success,
                    "failure" => GitHubStatusState::Failure,
                    "error" => GitHubStatusState::Error,
                    _ => GitHubStatusState::Pending,
                });
                
                let result = self.post_status(owner.clone(), repo.clone(), sha.clone(), state_enum.clone(), description.clone(), target_url.clone(), None).await?;
                
                let mut task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("github-status-{}-{}", repo, sha)),
                    task_type: TaskType::PostGitHubStatus,
                    status: TaskStatus::Succeeded,
                    priority: 60,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                
                let status_url = result.clone().unwrap_or_default();
                if let Some(url) = result {
                    task.metadata.insert("status_url".to_string(), url);
                }
                
                self.task_store.create_task(&task).await?;
                
                let state_str = state_enum.clone().map(|s| s.to_string()).unwrap_or_else(|| "pending".to_string());
                self.sender.send(AgentMessage::GitHubStatusPosted {
                    owner,
                    repo,
                    sha,
                    state: state_str,
                    status_url,
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::GitHubStatusPosted { .. } => {
                // Response message, ignore
            }
            
            AgentMessage::GitHubStatusFailed { .. } => {
                // Error message, ignore
            }
            
            _ => {
                tracing::debug!("Unhandled message: {:?}", message);
            }
        }
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentflow_core::agent::AgentDefinition;
    use agentflow_core::state::MemoryTaskStore;
    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use std::sync::Arc;
    
    struct MockStateStore;
    
    #[async_trait::async_trait]
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
    fn test_agent_creation() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = GitHubStatusAgent::new(sender, task_store, state_store, None);
        
        assert_eq!(agent.name(), "GitHubStatusAgent");
        assert!(agent.capabilities().contains("github-status"));
    }
    
    #[tokio::test]
    async fn test_status_enum_display() {
        assert_eq!(format!("{}", GitHubStatusState::Pending), "pending");
        assert_eq!(format!("{}", GitHubStatusState::Success), "success");
        assert_eq!(format!("{}", GitHubStatusState::Failure), "failure");
        assert_eq!(format!("{}", GitHubStatusState::Error), "error");
    }
    
    #[test]
    fn test_default_templates() {
        let templates = StatusTemplates::default();
        assert!(!templates.pending.is_empty());
        assert!(!templates.success.is_empty());
        assert!(!templates.failure.is_empty());
    }
}
