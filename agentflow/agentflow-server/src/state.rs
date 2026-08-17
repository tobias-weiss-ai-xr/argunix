//! Application state for the AgentFlow server

use agentflow_core::{
    SystemState,
    agent::{TaskStore, StateStore},
};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::config::ServerConfig;
use crate::AgentMessage;

/// Application state shared across all routes
#[derive(Clone)]
pub struct AppState {
    /// Message sender for communicating with agents
    pub sender: mpsc::Sender<AgentMessage>,
    
    /// System state with stores
    pub system_state: Arc<SystemState>,
    
    /// Task store
    pub task_store: Arc<dyn TaskStore + Send + Sync>,
    
    /// State store (for agents)
    pub agent_store: Arc<dyn StateStore + Send + Sync>,
    
    /// Server configuration
    pub config: ServerConfig,
    
    /// Server start time
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl AppState {
    /// Create a new AppState
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        system_state: Arc<SystemState>,
        config: ServerConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            sender,
            system_state: system_state.clone(),
            task_store: system_state.task_store.clone(),
            agent_store: system_state.agent_store.clone(),
            config,
            started_at: chrono::Utc::now(),
        })
    }
    
    /// Get the NATS message bus sender (if configured)
    #[allow(dead_code)]
    pub fn nats_sender(&self) -> Option<mpsc::Sender<AgentMessage>> {
        if self.config.is_distributed() {
            Some(self.sender.clone())
        } else {
            None
        }
    }
    
    /// Check if distributed mode is enabled
    #[allow(dead_code)]
    pub fn is_distributed(&self) -> bool {
        self.config.is_distributed()
    }
    
    /// Get server uptime
    pub fn uptime(&self) -> std::time::Duration {
        let duration = chrono::Utc::now() - self.started_at;
        std::time::Duration::from_secs(duration.num_seconds() as u64)
    }
}


