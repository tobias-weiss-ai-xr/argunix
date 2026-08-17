use crate::{Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use strum_macros::{Display, EnumString};
use tokio::sync::mpsc;
use chrono::{DateTime, Utc};

/// Types of agents in the system
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Display, EnumString)]
pub enum AgentType {
    // Control plane agents
    #[strum(serialize = "planner")]
    Planner,
    #[strum(serialize = "scheduler")]
    Scheduler,
    #[strum(serialize = "monitor")]
    Monitor,
    #[strum(serialize = "orchestrator")]
    Orchestrator,
    
    // Nix-related agents (argunix-inspired)
    #[strum(serialize = "flake-analyzer")]
    FlakeAnalyzer,
    #[strum(serialize = "dependency-graph")]
    DependencyGraph,
    #[strum(serialize = "security-gate")]
    SecurityGate,
    #[strum(serialize = "cache-manager")]
    CacheManager,
    #[strum(serialize = "nix-executor")]
    NixExecutor,
    #[strum(serialize = "builder")]
    Builder,
    
    // AI agents
    #[strum(serialize = "ai-code-reviewer")]
    AICodeReviewer,
    #[strum(serialize = "ai-flake-analyzer")]
    AIFlakeAnalyzer,
    #[strum(serialize = "ai-planner")]
    AIPlanner,
    #[strum(serialize = "ai-quality-gate")]
    AIQualityGate,
    
    // Mœ sovereignty agents
    #[strum(serialize = "identity-manager")]
    IdentityManager,
    #[strum(serialize = "storage-manager")]
    StorageManager,
    #[strum(serialize = "consensus-manager")]
    ConsensusManager,
    #[strum(serialize = "discovery")]
    Discovery,
    
    // Generic
    #[strum(serialize = "custom")]
    Custom,
}

/// Agent status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Display, EnumString)]
pub enum AgentStatus {
    #[strum(serialize = "starting")]
    Starting,
    #[strum(serialize = "ready")]
    Ready,
    #[strum(serialize = "busy")]
    Busy,
    #[strum(serialize = "draining")]
    Draining,
    #[strum(serialize = "stopping")]
    Stopping,
    #[strum(serialize = "error")]
    Error,
    #[strum(serialize = "offline")]
    Offline,
}

/// Agent definition
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder)]
#[builder(default)]
pub struct AgentDefinition {
    /// Unique agent identifier
    pub id: String,
    
    /// Agent name
    pub name: String,
    
    /// Agent type
    pub agent_type: AgentType,
    
    /// Current status
    #[builder(setter(skip))]
    pub status: AgentStatus,
    
    /// Agent capabilities
    #[builder(default)]
    pub capabilities: HashSet<String>,
    
    /// Maximum concurrent tasks
    #[builder(default = "10")]
    pub max_tasks: u32,
    
    /// Current active tasks
    #[builder(setter(skip))]
    pub active_tasks: u32,
    
    /// Resources available to this agent
    pub resources: Option<crate::task::ResourceRequirements>,
    
    /// Sovereign identity (Mœ concept)
    pub identity: Option<SovereignIdentity>,
    
    /// Configuration
    #[builder(default)]
    pub config: serde_json::Value,
    
    /// Last heartbeat timestamp
    #[builder(setter(skip))]
    pub last_heartbeat: Option<DateTime<Utc>>,
    
    /// Tasks completed count
    #[builder(setter(skip), default)]
    pub tasks_completed: u64,
    
    /// Tasks failed count
    #[builder(setter(skip), default)]
    pub tasks_failed: u64,
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "default".to_string(),
            agent_type: AgentType::Custom,
            status: AgentStatus::Starting,
            capabilities: HashSet::new(),
            max_tasks: 10,
            active_tasks: 0,
            resources: None,
            identity: None,
            config: serde_json::Value::Null,
            last_heartbeat: None,
            tasks_completed: 0,
            tasks_failed: 0,
        }
    }
}

/// Sovereign identity (Mœ-inspired)
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder)]
#[builder(default)]
pub struct SovereignIdentity {
    /// Public key (ed25519 base64)
    pub public_key: String,
    
    /// Key fingerprint (SHA256 of public key)
    pub fingerprint: String,
    
    /// Node/agent name
    pub name: String,
    
    /// Node type
    pub node_type: String,
    
    /// Capabilities
    #[builder(default)]
    pub capabilities: Vec<String>,
    
    /// Generation (Mœ multi-generational)
    #[builder(default)]
    pub generation: u64,
    
    /// Creation timestamp
    #[builder(setter(skip))]
    pub created_at: DateTime<Utc>,
    
    /// Expiration timestamp
    pub expires_at: Option<DateTime<Utc>>,
}

impl Default for SovereignIdentity {
    fn default() -> Self {
        Self {
            public_key: "".to_string(),
            fingerprint: "".to_string(),
            name: "default".to_string(),
            node_type: "default".to_string(),
            capabilities: vec![],
            generation: 0,
            created_at: Utc::now(),
            expires_at: None,
        }
    }
}

/// Agent trait - all agents must implement this
#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    /// Get agent name
    fn name(&self) -> &str;
    
    /// Get agent type
    fn agent_type(&self) -> AgentType;
    
    /// Get capabilities
    fn capabilities(&self) -> &HashSet<String>;
    
    /// Handle incoming message
    async fn handle_message(
        &mut self,
        message: crate::message::AgentMessage,
        ctx: &AgentContext,
    ) -> Result<()>;
    
    /// Called when agent starts
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> {
        Ok(())
    }
    
    /// Called when agent stops
    async fn on_shutdown(&mut self) -> Result<()> {
        Ok(())
    }
    
    /// Get agent status
    fn status(&self) -> AgentStatus {
        AgentStatus::Ready
    }
}

/// Agent context - provides access to system services
#[derive(Clone)]
pub struct AgentContext {
    /// Message bus sender
    pub sender: mpsc::Sender<crate::message::AgentMessage>,
    
    /// Agent definition
    pub agent_def: AgentDefinition,
    
    /// Task store
    pub task_store: Option<Arc<dyn crate::agent::TaskStore + Send + Sync>>,
    
    /// State store
    pub state_store: Option<Arc<dyn crate::agent::StateStore + Send + Sync>>,
}

impl AgentContext {
    pub fn new(
        sender: mpsc::Sender<crate::message::AgentMessage>,
        agent_def: AgentDefinition,
        task_store: Option<Arc<dyn crate::agent::TaskStore + Send + Sync>>,
        state_store: Option<Arc<dyn crate::agent::StateStore + Send + Sync>>,
    ) -> Self {
        Self {
            sender,
            agent_def,
            task_store,
            state_store,
        }
    }
}

/// Task store trait
#[async_trait::async_trait]
pub trait TaskStore: Send + Sync {
    async fn create_task(&self, task: &crate::task::TaskDefinition) -> Result<crate::task::TaskDefinition>;
    async fn get_task(&self, id: &str) -> Result<Option<crate::task::TaskDefinition>>;
    async fn update_task(&self, id: &str, update: TaskUpdate) -> Result<crate::task::TaskDefinition>;
    async fn list_tasks(&self, filter: Option<TaskFilter>) -> Result<Vec<crate::task::TaskDefinition>>;
    async fn delete_task(&self, id: &str) -> Result<()>;
}

/// State store trait
#[async_trait::async_trait]
pub trait StateStore: Send + Sync {
    async fn get_agent(&self, id: &str) -> Result<Option<AgentDefinition>>;
    async fn register_agent(&self, agent: &AgentDefinition) -> Result<()>;
    async fn deregister_agent(&self, id: &str) -> Result<()>;
    async fn list_agents(&self) -> Result<Vec<AgentDefinition>>;
}

/// Task filter for listing
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub status: Option<Vec<crate::task::TaskStatus>>,
    pub task_type: Option<Vec<crate::task::TaskType>>,
    pub priority_min: Option<crate::task::Priority>,
    pub priority_max: Option<crate::task::Priority>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub flake_url: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Task update
#[derive(Debug, Clone, Default)]
pub struct TaskUpdate {
    pub status: Option<crate::task::TaskStatus>,
    pub priority: Option<crate::task::Priority>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub metadata: Option<std::collections::HashMap<String, String>>,
}
