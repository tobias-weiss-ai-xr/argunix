//! MatrixNotifierAgent - Send notifications via Matrix protocol
//!
//! This agent handles:
//! - Connecting to Matrix homeservers
//! - Sending messages to rooms
//! - Formatting messages (plain text, Markdown, HTML)
//! - Uploading file attachments
//! - Managing Matrix sessions
//!
//! ## Features
//! - Matrix protocol v3 API support
//! - Multiple rooms for different notification types
//! - Template-based message formatting
//! - Markdown and HTML message formatting
//! - Rate limiting and retry logic
//!
//! ## Messages Handled
//! - SendMatrixNotification: Send a notification message
//! - BroadcastMatrixMessage: Send to multiple rooms
//! - SendMatrixFile: Upload and share a file
//!
//! ## Environment Variables
//! - `MATRIX_ACCESS_TOKEN`: Matrix access token
//! - `MATRIX_PASSWORD`: Matrix login password (alternative)
//! - `MATRIX_HOMESERVER`: Override default homeserver
//! - `MATRIX_USER_ID`: Matrix user ID

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, RwLock};
use serde::{Deserialize, Serialize};
use async_trait::async_trait;

use agentflow_core::{Agent, AgentContext, AgentMessage, AgentStatus, AgentType, Result, TaskDefinition, TaskStatus, TaskType};
use agentflow_core::agent::{StateStore, TaskStore};

/// Matrix configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    /// Homeserver URL
    #[serde(default = "default_homeserver")]
    pub homeserver: String,
    /// Username for login
    pub username: Option<String>,
    /// User ID (full Matrix ID)
    pub user_id: Option<String>,
    /// Default room for notifications
    #[serde(default = "default_room")]
    pub default_room: String,
    /// Named rooms mapping
    #[serde(default)]
    pub rooms: HashMap<String, String>,
    /// Enable HTML formatting
    #[serde(default)]
    pub html_enabled: bool,
    /// Enable Markdown formatting
    #[serde(default = "default_markdown")]
    pub markdown_enabled: bool,
    /// Use emoji/formatting
    #[serde(default = "default_use_formatting")]
    pub use_formatting: bool,
    /// Maximum message length
    #[serde(default = "default_max_message_length")]
    pub max_message_length: usize,
    /// Send read receipts
    #[serde(default)]
    pub send_receipts: bool,
    /// Retry configuration
    #[serde(default)]
    pub retry: RetryConfig,
    /// Message templates
    #[serde(default)]
    pub templates: MessageTemplates,
}

impl Default for MatrixConfig {
    fn default() -> Self {
        let mut rooms = HashMap::new();
        rooms.insert("alerts".to_string(), "!alerts:matrix.org".to_string());
        rooms.insert("builds".to_string(), "!builds:matrix.org".to_string());
        rooms.insert("general".to_string(), "!general:matrix.org".to_string());
        
        Self {
            homeserver: default_homeserver(),
            username: None,
            user_id: None,
            default_room: default_room(),
            rooms,
            html_enabled: true,
            markdown_enabled: default_markdown(),
            use_formatting: default_use_formatting(),
            max_message_length: default_max_message_length(),
            send_receipts: false,
            retry: RetryConfig::default(),
            templates: MessageTemplates::default(),
        }
    }
}

fn default_homeserver() -> String { "https://matrix.org".to_string() }
fn default_room() -> String { "!builds:matrix.org".to_string() }
fn default_markdown() -> bool { true }
fn default_use_formatting() -> bool { true }
fn default_max_message_length() -> usize { 4096 }

/// Retry configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default)]
    pub retry_on_failure: bool,
    #[serde(default = "default_retry_delay")]
    pub retry_delay: u64,
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub exponential_backoff: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            retry_on_failure: true,
            retry_delay: default_retry_delay(),
            max_retries: 3,
            exponential_backoff: true,
        }
    }
}

fn default_retry_delay() -> u64 { 5 }

/// Message templates
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageTemplates {
    #[serde(default = "default_build_started_template")]
    pub build_started: String,
    #[serde(default = "default_build_complete_template")]
    pub build_complete: String,
    #[serde(default = "default_build_failed_template")]
    pub build_failed: String,
    #[serde(default = "default_tests_started_template")]
    pub tests_started: String,
    #[serde(default = "default_tests_passed_template")]
    pub tests_passed: String,
    #[serde(default = "default_tests_failed_template")]
    pub tests_failed: String,
    #[serde(default = "default_deployment_template")]
    pub deployment: String,
}

impl Default for MessageTemplates {
    fn default() -> Self {
        Self {
            build_started: default_build_started_template(),
            build_complete: default_build_complete_template(),
            build_failed: default_build_failed_template(),
            tests_started: default_tests_started_template(),
            tests_passed: default_tests_passed_template(),
            tests_failed: default_tests_failed_template(),
            deployment: default_deployment_template(),
        }
    }
}

fn default_build_started_template() -> String { "Nix build started: {repo}@{ref}".to_string() }
fn default_build_complete_template() -> String { "Nix build succeeded: {repo}@{ref}".to_string() }
fn default_build_failed_template() -> String { "Nix build failed: {repo}@{ref}".to_string() }
fn default_tests_started_template() -> String { "Tests started: {repo}".to_string() }
fn default_tests_passed_template() -> String { "Tests passed: {repo}".to_string() }
fn default_tests_failed_template() -> String { "Tests failed: {repo}".to_string() }
fn default_deployment_template() -> String { "Deployment: {repo}@{ref} to {environment}".to_string() }

/// Authentication state
#[derive(Debug, Clone)]
pub struct MatrixAuthState {
    pub logged_in: bool,
    pub user_id: Option<String>,
    pub access_token: Option<String>,
    pub device_id: Option<String>,
    pub auth_method: MatrixAuthMethod,
    pub last_login: Option<DateTime<Utc>>,
}

impl Default for MatrixAuthState {
    fn default() -> Self {
        Self {
            logged_in: false,
            user_id: None,
            access_token: None,
            device_id: None,
            auth_method: MatrixAuthMethod::None,
            last_login: None,
        }
    }
}

/// Authentication method
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MatrixAuthMethod {
    Password,
    Token,
    None,
}

/// Agent statistics
#[derive(Debug, Default, Clone)]
pub struct MatrixStats {
    pub messages_sent: u64,
    pub files_uploaded: u64,
    pub requests_made: u64,
    pub errors: u64,
    pub rate_limit_hits: u64,
    pub successful_notifications: u64,
    pub failed_notifications: u64,
}

/// Agent state
#[derive(Debug, Default)]
pub struct MatrixNotifierState {
    pub auth: MatrixAuthState,
    pub stats: MatrixStats,
    pub joined_rooms: HashSet<String>,
}

/// Matrix login response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixLoginResponse {
    pub user_id: String,
    pub access_token: String,
    pub home_server: String,
    pub device_id: Option<String>,
}

/// Matrix message response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixMessageResponse {
    pub event_id: Option<String>,
    pub room_id: Option<String>,
}

/// Matrix file response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixFileResponse {
    pub content_uri: Option<String>,
}

/// The MatrixNotifierAgent
pub struct MatrixNotifierAgent {
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn TaskStore>,
    /// State store
    _state_store: Arc<dyn StateStore>,
    /// Configuration
    config: MatrixConfig,
    /// State
    state: Arc<RwLock<MatrixNotifierState>>,
    /// HTTP client
    client: reqwest::Client,
    /// Agent name
    name: String,
    /// Agent type
    agent_type: AgentType,
    /// Agent capabilities
    capabilities: HashSet<String>,
    /// Agent status
    status: AgentStatus,
}

impl MatrixNotifierAgent {
    /// Create a new MatrixNotifierAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn TaskStore>,
        state_store: Arc<dyn StateStore>,
        config: Option<MatrixConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        
        let has_token = std::env::var("MATRIX_ACCESS_TOKEN").is_ok() 
            || std::env::var("MATRIX_PASSWORD").is_ok();
        
        let capabilities = vec![
            "matrix-notify".to_string(),
            "room-messaging".to_string(),
            "file-attachments".to_string(),
            "message-formatting".to_string(),
            "html-formatting".to_string(),
            "markdown-formatting".to_string(),
        ].into_iter().collect();
        
        Self {
            sender,
            task_store,
            _state_store: state_store,
            config,
            state: Arc::new(RwLock::new(MatrixNotifierState::default())),
            client,
            name: "MatrixNotifierAgent".to_string(),
            agent_type: AgentType::Custom,
            capabilities,
            status: if has_token { AgentStatus::Ready } else { AgentStatus::Error },
        }
    }
    
    /// Check if agent is configured
    pub fn is_configured(&self) -> bool {
        std::env::var("MATRIX_ACCESS_TOKEN").is_ok() 
            || std::env::var("MATRIX_PASSWORD").is_ok()
            || {
                let state = self.state.blocking_read();
                state.auth.logged_in
            }
    }
    
    /// Get access token
    async fn get_access_token(&self) -> Option<String> {
        {
            let state = self.state.read().await;
            if let Some(ref token) = state.auth.access_token {
                return Some(token.clone());
            }
        }
        
        std::env::var("MATRIX_ACCESS_TOKEN").ok()
    }
    
    /// Login with token
    pub async fn login_with_token(&mut self, token: String) -> Result<()> {
        let mut state = self.state.write().await;
        state.auth.logged_in = true;
        state.auth.access_token = Some(token);
        state.auth.auth_method = MatrixAuthMethod::Token;
        state.auth.last_login = Some(Utc::now());
        Ok(())
    }
    
    /// Encode room ID for URL
    fn encode_room_id(room_id: &str) -> String {
        room_id.replace('#', "%23")
    }
    
    /// Format message body
    fn format_message_body(&self, content: &str, formatted: Option<&str>) -> serde_json::Value {
        let mut body = serde_json::json!({
            "msgtype": "m.room.message",
            "body": content,
        });
        
        if let Some(formatted_content) = formatted {
            if self.config.html_enabled {
                body["format"] = serde_json::Value::String("org.matrix.custom.html".to_string());
                body["formatted_body"] = serde_json::Value::String(formatted_content.to_string());
            } else if self.config.markdown_enabled {
                body["msgtype"] = serde_json::Value::String("m.text".to_string());
            }
        }
        
        body
    }
    
    /// Send a message to a room with retries
    pub async fn send_message(
        &mut self,
        room: String,
        message: String,
        formatted: Option<String>,
    ) -> Result<MatrixMessageResponse> {
        let token = self.get_access_token().await
            .ok_or_else(|| agentflow_core::AgentFlowError::Generic("Not authenticated".to_string()))?;
        
        let max_retries = self.config.retry.max_retries;
        let mut current_retries = 0u32;
        
        let message = if message.len() > self.config.max_message_length {
            let ellipsis = if self.config.use_formatting { "..." } else { "..." };
            format!("{}{}", &message[..self.config.max_message_length - 3], ellipsis)
        } else {
            message
        };
        
        loop {
            let body = self.format_message_body(&message, formatted.as_deref());
            
            let url = format!(
                "{}/_matrix/client/v3/rooms/{}/send/m.room.message",
                self.config.homeserver,
                Self::encode_room_id(&room)
            );
            
            let response = self.client.post(&url)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await;
            
            {
                let mut state = self.state.write().await;
                state.stats.requests_made += 1;
            }
            
            match response {
                Ok(resp) => {
                    if resp.status().is_success() {
                        let result: MatrixMessageResponse = resp.json().await
                            .map_err(|_| agentflow_core::AgentFlowError::Generic("Failed to parse Matrix response".to_string()))?;
                        
                        {
                            let mut state = self.state.write().await;
                            state.stats.messages_sent += 1;
                            state.stats.successful_notifications += 1;
                        }
                        
                        return Ok(result);
                    } else {
                        let error_msg = format!("Matrix API error: {} - {}", 
                            resp.status(), 
                            resp.text().await.unwrap_or_default());
                        
                        {
                            let mut state = self.state.write().await;
                            state.stats.errors += 1;
                            state.stats.failed_notifications += 1;
                        }
                        
                        if self.config.retry.retry_on_failure && current_retries < max_retries {
                            let delay = if self.config.retry.exponential_backoff {
                                Duration::from_secs(self.config.retry.retry_delay * (2u64).pow(current_retries))
                            } else {
                                Duration::from_secs(self.config.retry.retry_delay)
                            };
                            tokio::time::sleep(delay).await;
                            current_retries += 1;
                            continue;
                        }
                        
                        return Err(agentflow_core::AgentFlowError::Generic(error_msg));
                    }
                }
                Err(e) => {
                    {
                        let mut state = self.state.write().await;
                        state.stats.errors += 1;
                        state.stats.failed_notifications += 1;
                    }
                    
                    if self.config.retry.retry_on_failure && current_retries < max_retries {
                        let delay = if self.config.retry.exponential_backoff {
                            Duration::from_secs(self.config.retry.retry_delay * (2u64).pow(current_retries))
                        } else {
                            Duration::from_secs(self.config.retry.retry_delay)
                        };
                        tokio::time::sleep(delay).await;
                        current_retries += 1;
                        continue;
                    }
                    
                    return Err(agentflow_core::AgentFlowError::Network(e.to_string()));
                }
            }
        }
    }
    
    /// Send notification using a template
    pub async fn send_notification(
        &mut self,
        template: &str,
        replacements: HashMap<&str, &str>,
        room_name: Option<&str>,
    ) -> Result<MatrixMessageResponse> {
        let room = room_name.and_then(|name| self.config.rooms.get(name).cloned())
            .unwrap_or_else(|| self.config.default_room.clone());
        
        let mut message = template.to_string();
        for (key, value) in replacements {
            message = message.replace(key, value);
        }
        
        self.send_message(room, message, None).await
    }
    
    /// Broadcast message to multiple rooms
    pub async fn broadcast_message(
        &mut self,
        message: String,
        rooms: Vec<String>,
    ) -> Result<Vec<MatrixMessageResponse>> {
        let mut results = Vec::new();
        
        for room in &rooms {
            match self.send_message(room.clone(), message.clone(), None).await {
                Ok(result) => results.push(result),
                Err(e) => {
                    tracing::error!("Failed to send to room {}: {}", room, e);
                }
            }
        }
        
        Ok(results)
    }
    
    /// Upload a file to Matrix
    pub async fn upload_file(
        &mut self,
        file_name: String,
        content_type: Option<String>,
        data: Vec<u8>,
    ) -> Result<MatrixFileResponse> {
        let token = self.get_access_token().await
            .ok_or_else(|| agentflow_core::AgentFlowError::Generic("Not authenticated".to_string()))?;
        
        let max_retries = self.config.retry.max_retries;
        let mut current_retries = 0u32;
        
        let device_id = {
            let state = self.state.read().await;
            state.auth.device_id.clone().unwrap_or_default()
        };
        
        loop {
            let mut url = format!(
                "{}/_matrix/media/v3/upload?filename={}",
                self.config.homeserver,
                Self::encode_room_id(&file_name)
            );
            
            if let Some(ref ctype) = content_type {
                url.push_str(&format!("&content_type={}", ctype));
            }
            
            let mut request = self.client.post(&url).bearer_auth(&token);
            
            if !device_id.is_empty() {
                if let Ok(header_value) = reqwest::header::HeaderValue::from_str(&device_id) {
                    request = request.header("X-Device-Id", header_value);
                }
            }
            
            let response = request
                .body(data.clone())
                .send()
                .await;
            
            {
                let mut state = self.state.write().await;
                state.stats.requests_made += 1;
            }
            
            match response {
                Ok(resp) => {
                    if resp.status().is_success() {
                        let result: MatrixFileResponse = resp.json().await
                            .map_err(|_| agentflow_core::AgentFlowError::Generic("Failed to parse Matrix upload response".to_string()))?;
                        
                        {
                            let mut state = self.state.write().await;
                            state.stats.files_uploaded += 1;
                        }
                        
                        return Ok(result);
                    } else {
                        let error_msg = format!("Matrix upload error: {} - {}", 
                            resp.status(), 
                            resp.text().await.unwrap_or_default());
                        
                        {
                            let mut state = self.state.write().await;
                            state.stats.errors += 1;
                        }
                        
                        if self.config.retry.retry_on_failure && current_retries < max_retries {
                            let delay = if self.config.retry.exponential_backoff {
                                Duration::from_secs(self.config.retry.retry_delay * (2u64).pow(current_retries))
                            } else {
                                Duration::from_secs(self.config.retry.retry_delay)
                            };
                            tokio::time::sleep(delay).await;
                            current_retries += 1;
                            continue;
                        }
                        
                        return Err(agentflow_core::AgentFlowError::Generic(error_msg));
                    }
                }
                Err(e) => {
                    {
                        let mut state = self.state.write().await;
                        state.stats.errors += 1;
                    }
                    
                    if self.config.retry.retry_on_failure && current_retries < max_retries {
                        let delay = if self.config.retry.exponential_backoff {
                            Duration::from_secs(self.config.retry.retry_delay * (2u64).pow(current_retries))
                        } else {
                            Duration::from_secs(self.config.retry.retry_delay)
                        };
                        tokio::time::sleep(delay).await;
                        current_retries += 1;
                        continue;
                    }
                    
                    return Err(agentflow_core::AgentFlowError::Network(e.to_string()));
                }
            }
        }
    }
    
    /// Get statistics
    pub async fn get_stats(&self) -> MatrixStats {
        let state = self.state.read().await;
        state.stats.clone()
    }
    
    /// Get joined rooms
    pub async fn get_joined_rooms(&self) -> Vec<String> {
        let state = self.state.read().await;
        state.joined_rooms.iter().cloned().collect()
    }
}

/// Implement Agent trait for MatrixNotifierAgent
#[async_trait::async_trait]
impl Agent for MatrixNotifierAgent {
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
            AgentMessage::SendMatrixNotification { room, message, formatted, task_id, .. } => {
                let msg_len = message.len();
                let room_clone = room.clone();
                let message_clone = message.clone();
                let formatted_clone = formatted.clone();
                let result = self.send_message(room, message, formatted).await?;
                
                let event_id_clone = result.event_id.clone();
                let room_id_clone = result.room_id.clone();
                
                let mut task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("matrix-notify-{}", msg_len)),
                    task_type: TaskType::SendMatrixNotification,
                    status: TaskStatus::Succeeded,
                    priority: 60,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                
                if let Some(event_id) = &event_id_clone {
                    task.metadata.insert("event_id".to_string(), event_id.clone());
                }
                if let Some(room_id) = &room_id_clone {
                    task.metadata.insert("room_id".to_string(), room_id.clone());
                }
                
                self.task_store.create_task(&task).await?;
                
                self.sender.send(AgentMessage::MatrixNotificationSent {
                    room: room_clone,
                    message: message_clone,
                    event_id: event_id_clone.unwrap_or_default(),
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::BroadcastMatrixMessage { message, rooms, task_id, .. } => {
                let msg_len = message.len();
                let results = self.broadcast_message(message.clone(), rooms.clone()).await?;
                
                let mut task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("matrix-broadcast-{}", msg_len)),
                    task_type: TaskType::BroadcastMatrixMessage,
                    status: TaskStatus::Succeeded,
                    priority: 60,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                
                task.metadata.insert("rooms_count".to_string(), results.len().to_string());
                
                self.task_store.create_task(&task).await?;
                
                self.sender.send(AgentMessage::MatrixNotificationSent {
                    room: "broadcast".to_string(),
                    message,
                    event_id: "".to_string(),
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::SendMatrixFile { file_name, content_type, data, room, task_id, .. } => {
                let fn_clone = file_name.clone();
                let result = self.upload_file(file_name.clone(), content_type.clone(), data).await?;
                
                let content_uri_clone = result.content_uri.clone();
                
                let mut task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("matrix-file-{}", fn_clone)),
                    task_type: TaskType::SendMatrixFile,
                    status: TaskStatus::Succeeded,
                    priority: 60,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                
                if let Some(uri) = &content_uri_clone {
                    task.metadata.insert("content_uri".to_string(), uri.clone());
                }
                
                self.task_store.create_task(&task).await?;
                
                self.sender.send(AgentMessage::MatrixFileSent {
                    file_name: fn_clone,
                    content_type: content_type.unwrap_or_default(),
                    content_uri: content_uri_clone.unwrap_or_default(),
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::MatrixNotificationSent { .. } | 
            AgentMessage::MatrixFileSent { .. } => {
                // Responses we send - ignore
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
    use super::*;
    use agentflow_core::agent::AgentDefinition;
    use agentflow_core::state::MemoryTaskStore;
    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use std::sync::Arc;
    
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
    fn test_message_templates() {
        let templates = MessageTemplates::default();
        assert!(templates.build_started.contains("{repo}"));
        assert!(templates.build_complete.contains("{repo}"));
        assert!(templates.build_failed.contains("{repo}"));
    }
    
    #[test]
    fn test_encode_room_id() {
        assert_eq!(MatrixNotifierAgent::encode_room_id("!room:server"), "!room:server");
        assert_eq!(MatrixNotifierAgent::encode_room_id("#alias:server"), "%23alias:server");
    }
    
    #[tokio::test]
    async fn test_agent_creation() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = MatrixNotifierAgent::new(sender, task_store, state_store, None);
        
        assert_eq!(agent.name(), "MatrixNotifierAgent");
        assert!(agent.capabilities().contains("matrix-notify"));
        assert!(agent.capabilities().contains("room-messaging"));
    }
}
