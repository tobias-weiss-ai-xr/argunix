# AgentFlow Quick Start Guide

<!--
SPDX-FileCopyrightText: 2026 AgentFlow Contributors
SPDX-License-Identifier: Apache-2.0
-->

This guide gets you started with **AgentFlow** in 5 minutes. We'll create a minimal working version that you can iterate on.

## Prerequisites

- **Rust 1.75+** (for async/await, serde, tokio)
- **Nix** (optional, but recommended for reproducibility)
- **Git**
- **Docker** (optional, for containerized builds)

## Step 0: Setup Repository

```bash
# Create the agentflow repository
mkdir -p ~/git/agentflow
cd ~/git/agentflow

# Initialize git
git init
git config user.name "Your Name"
git config user.email "your@email.com"

# Create license
echo "SPDX-FileCopyrightText: 2026 AgentFlow Contributors" > LICENSE
echo "SPDX-License-Identifier: Apache-2.0" >> LICENSE

# Create README
cat > README.md << 'EOF'
# AgentFlow / TaskFleet

Sovereign Agent-Driven CI/CD Platform

Combines:
- argunix's Nix-native CI concepts
- Mœ Sovereignty's self-sovereign computing
- Intelligent agent orchestration

## Quick Start

```bash
cargo run --release
```

See [AGENTFLOW-QUICKSTART.md](AGENTFLOW-QUICKSTART.md) for details.
EOF

git add LICENSE README.md
git commit -m "Initial commit: License and README"

# Initialize Cargo workspace
cat > Cargo.toml << 'EOF'
[workspace]
members = [
    "agentflow-core",
    "agentflow-agents",
    "agentflow-cli",
    "agentflow-server",
    "agentflow-storage",
]
resolver = "2"
EOF

# Create directory structure
mkdir -p agentflow-{core,agents,cli,server,storage}/src

# Copy design docs (optional, for reference)
# cp ~/git/argunix/AGENTFLOW-*.md docs/

git add Cargo.toml
git commit -m "Add Cargo workspace structure"
```

## Step 1: Core Types (agentflow-core)

```bash
cd ~/git/agentflow

# agentflow-core/Cargo.toml
cat > agentflow-core/Cargo.toml << 'EOF'
[package]
name = "agentflow-core"
version = "0.1.0"
edition = "2021"
authors = ["Tobias Weiss <weissto@hrz.uni-marburg.de>"]
license = "Apache-2.0"
description = "Core types and traits for AgentFlow"

[dependencies]
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tokio = { version = "1.0", features = ["full"] }
anyhow = "1.0"
derive-new = "0.6"
derive-builder = "0.12"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1.0", features = ["v4", "serde"] }
strum = { version = "0.25", features = ["derive"] }
strum_macros = "0.25"

[dev-dependencies]
tokio-test = "0.4"
EOF

# agentflow-core/src/lib.rs
cat > agentflow-core/src/lib.rs << 'EOF'
//! Core types and traits for AgentFlow

pub mod error;
pub mod message;
pub mod state;
pub mod task;
pub mod agent;

// Re-export main types
pub use agent::*;
pub use error::*;
pub use message::*;
pub use state::*;
pub use task::*;
EOF

# agentflow-core/src/error.rs
cat > agentflow-core/src/error.rs << 'EOF'
use thiserror::Error;

/// Top-level error type for AgentFlow
#[derive(Debug, Error)]
pub enum AgentFlowError {
    /// Generic error with message
    #[error("{0}")]
    Generic(String),
    
    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    /// Serde serialization error
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    
    /// Task-specific error
    #[error("Task error: {0}")]
    Task(String),
    
    /// Agent error
    #[error("Agent error: {0}")]
    Agent(String),
    
    /// Storage error
    #[error("Storage error: {0}")]
    Storage(String),
    
    /// Network error
    #[error("Network error: {0}")]
    Network(String),
    
    /// Authentication/authorization error
    #[error("Auth error: {0}")]
    Auth(String),
    
    /// Not found
    #[error("{0} not found")]
    NotFound(String),
    
    /// Already exists
    #[error("{0} already exists")]
    AlreadyExists(String),
}

pub type Result<T, E = AgentFlowError> = std::result::Result<T, E>;
EOF

# agentflow-core/src/task.rs
cat > agentflow-core/src/task.rs << 'EOF'
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use strum_macros::{Display, EnumString};
use uuid::Uuid;

/// Types of tasks the system can execute
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Display, EnumString)]
pub enum TaskType {
    // Nix tasks (argunix-inspired)
    #[strum(serialize = "nix-eval")]
    NixEval,
    #[strum(serialize = "nix-build")]
    NixBuild,
    #[strum(serialize = "nix-check")]
    NixCheck,
    #[strum(serialize = "nix-devshell")]
    NixDevShell,
    #[strum(serialize = "nix-bundle")]
    NixBundle,
    
    // AI tasks
    #[strum(serialize = "ai-code-review")]
    AICodeReview,
    #[strum(serialize = "ai-flake-analysis")]
    AIFlakeAnalysis,
    #[strum(serialize = "ai-plan-generation")]
    AIPlanGeneration,
    
    // Mœ tasks
    #[strum(serialize = "moe-sync")]
    MoeSync,
    #[strum(serialize = "moe-verify")]
    MoeVerify,
    #[strum(serialize = "moe-gc")]
    MoeGC,
    
    // Generic
    #[strum(serialize = "custom-command")]
    CustomCommand,
    #[strum(serialize = "multi-task")]
    MultiTask,
}

/// Task status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Display, EnumString)]
pub enum TaskStatus {
    #[strum(serialize = "pending")]
    Pending,
    #[strum(serialize = "scheduled")]
    Scheduled,
    #[strum(serialize = "running")]
    Running,
    #[strum(serialize = "succeeded")]
    Succeeded,
    #[strum(serialize = "failed")]
    Failed,
    #[strum(serialize = "cancelled")]
    Cancelled,
    #[strum(serialize = "timeout")]
    Timeout,
}

/// Task priority (0 = lowest, 100 = highest)
pub type Priority = u8;

/// maximum number of priority levels
pub const MAX_PRIORITY: Priority = 100;

/// Task definition
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct TaskDefinition {
    /// Unique task identifier
    pub id: String,
    
    /// Task type
    pub task_type: TaskType,
    
    /// Task status
    #[builder(setter skip)]
    pub status: TaskStatus,
    
    /// Priority (0-100)
    #[builder(default = "50")]
    pub priority: Priority,
    
    /// Creation timestamp
    #[builder(setter skip)]
    pub created_at: DateTime<Utc>,
    
    /// Start timestamp
    #[builder(setter skip)]
    pub started_at: Option<DateTime<Utc>>,
    
    /// Completion timestamp
    #[builder(setter skip)]
    pub completed_at: Option<DateTime<Utc>>,
    
    // --- Nix-specific fields ---
    
    /// Flake URL
    pub flake_url: Option<String>,
    
    /// Flake reference (branch, tag, commit)
    pub flake_ref: Option<String>,
    
    /// Target system
    pub system: Option<String>,
    
    /// Target packages/checks/devShells to evaluate/build
    pub targets: Option<Vec<String>>,
    
    /// Derivation path
    pub drv_path: Option<String>,
    
    // --- AI-specific fields ---
    
    /// AI model to use
    pub model: Option<String>,
    
    /// Prompt for AI
    pub prompt: Option<String>,
    
    /// Handbooks/context files
    pub handbooks: Option<Vec<String>>,
    
    // --- Dependencies ---
    
    /// Task dependencies (by task ID)
    pub depends_on: Option<Vec<String>>,
    
    // --- Resources ---
    
    /// Required resources
    pub resources: Option<ResourceRequirements>,
    
    /// Constraints
    pub constraints: Option<TaskConstraints>,
    
    // --- metadata ---
    
    /// Arbitrary metadata
    #[builder(default)]
    pub metadata: HashMap<String, String>,
}

impl Default for TaskDefinition {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            task_type: TaskType::CustomCommand,
            status: TaskStatus::Pending,
            priority: 50,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            flake_url: None,
            flake_ref: None,
            system: None,
            targets: None,
            drv_path: None,
            model: None,
            prompt: None,
            handbooks: None,
            depends_on: None,
            resources: None,
            constraints: None,
            metadata: HashMap::new(),
        }
    }
}

/// Resource requirements for a task
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct ResourceRequirements {
    pub cpu: Option<f32>,           // CPU cores
    pub memory: Option<u64>,       // Memory in bytes
    pub storage: Option<u64>,      // Storage in bytes
    pub gpu: Option<u32>,          // Number of GPUs
    pub gpu_memory: Option<u64>,   // GPU memory in bytes
}

/// Task constraints
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct TaskConstraints {
    /// Required node labels
    pub node_labels: Option<HashMap<String, String>>,
    
    /// Affinity/anti-affinity rules
    pub affinity: Option<AffinityRules>,
    
    /// Maximum execution time in seconds
    pub timeout_seconds: Option<u64>,
    
    /// Retry policy
    pub retry_policy: Option<RetryPolicy>,
    
    /// Data locality requirements
    pub data_locality: Option<DataLocality>,
    
    /// Compliance tags
    pub compliance_tags: Option<Vec<String>>,
}

/// Affinity rules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AffinityType {
    Required,
    Preferred,
    AntiAffinity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffinityRule {
    pub affinity_type: AffinityType,
    pub key: String,
    pub values: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AffinityRules {
    pub rules: Vec<AffinityRule>,
}

/// Retry policy
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct RetryPolicy {
    pub max_attempts: Option<u32>,
    pub backoff_seconds: Option<u64>,
    pub backoff_multiplier: Option<f32>,
}

/// Data locality constraint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataLocality {
    Anywhere,
    Region(String),
    Zone(String),
    Node(String),
}

/// Task result
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct TaskResult {
    pub task_id: String,
    pub status: TaskStatus,
    pub output: Option<String>,
    pub artifacts: Option<Vec<Artifact>>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub duration_seconds: Option<f64>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub metadata: HashMap<String, String>,
}

/// Build artifact
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct Artifact {
    pub name: String,
    pub path: String,
    pub hash: Option<String>,
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub storage_location: Option<String>,
}
EOF

# agentflow-core/src/agent.rs
cat > agentflow-core/src/agent.rs << 'EOF'
use crate::{AgentFlowError, Result, TaskDefinition, TaskResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use strum_macros::{Display, EnumString};

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
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct AgentDefinition {
    /// Unique agent identifier
    pub id: String,
    
    /// Agent name
    pub name: String,
    
    /// Agent type
    pub agent_type: AgentType,
    
    /// Current status
    #[builder(setter skip)]
    pub status: AgentStatus,
    
    /// Agent capabilities
    #[builder(default)]
    pub capabilities: HashSet<String>,
    
    /// Maximum concurrent tasks
    #[builder(default = "10")]
    pub max_tasks: u32,
    
    /// Current active tasks
    #[builder(setter skip, default)]
    pub active_tasks: u32,
    
    /// Resources available to this agent
    pub resources: Option<crate::task::ResourceRequirements>,
    
    /// Sovereign identity (Mœ concept)
    pub identity: Option<SovereignIdentity>,
    
    /// Configuration
    #[builder(default)]
    pub config: serde_json::Value,
}

/// Sovereign identity (Mœ-inspired)
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
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
    #[builder(setter skip)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    
    /// Expiration timestamp
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
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
        &self,
        message: AgentMessage,
        ctx: &AgentContext,
    ) -> Result<()>;
    
    /// Called when agent starts
    async fn on_start(&mut self, ctx: &AgentContext) -> Result<()> {
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
#[derive(Debug, Clone)]
pub struct AgentContext {
    /// Message bus for agent communication
    pub bus: tokio::sync::mpsc::Sender<AgentMessage>,
    
    /// Task storage
    pub task_store: Option<std::sync::Arc<dyn TaskStore>>,
    
    /// State storage
    pub state_store: Option<std::sync::Arc<dyn StateStore>>,
    
    /// Agent ID
    pub agent_id: String,
    
    /// Configuration
    pub config: serde_json::Value,
    
    /// Logger
    pub log: slog::Logger,
}

/// Task store trait
#[async_trait::async_trait]
pub trait TaskStore: Send + Sync {
    async fn create_task(&self, task: &TaskDefinition) -> Result<TaskDefinition>;
    async fn get_task(&self, id: &str) -> Result<Option<TaskDefinition>>;
    async fn update_task(&self, id: &str, update: TaskUpdate) -> Result<TaskDefinition>;
    async fn list_tasks(&self, filter: Option<TaskFilter>) -> Result<Vec<TaskDefinition>>;
    async fn delete_task(&self, id: &str) -> Result<()>;
}

/// State store trait
#[async_trait::async_trait]
pub trait StateStore: Send + Sync {
    async fn get_agent(&self, id: &str) -> Result<Option<AgentDefinition>>;
    async fn register_agent(&self, agent: &AgentDefinition) -> Result<()>;
    async fn dereister_agent(&self, id: &str) -> Result<()>;
    async fn list_agents(&self) -> Result<Vec<AgentDefinition>>;
}

/// Task filter for listing
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub status: Option<Vec<crate::task::TaskStatus>>,
    pub task_type: Option<Vec<TaskType>>,
    pub priority_min: Option<crate::task::Priority>,
    pub priority_max: Option<crate::task::Priority>,
    pub created_after: Option<chrono::DateTime<chrono::Utc>>,
    pub created_before: Option<chrono::DateTime<chrono::Utc>>,
    pub flake_url: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Task update
#[derive(Debug, Clone, Default)]
pub struct TaskUpdate {
    pub status: Option<crate::task::TaskStatus>,
    pub priority: Option<crate::task::Priority>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub metadata: Option<std::collections::HashMap<String, String>>,
}
EOF

# agentflow-core/src/message.rs
cat > agentflow-core/src/message.rs << 'EOF'
use crate::{AgentDefinition, AgentType, TaskDefinition, TaskResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Messages for agent communication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessage {
    // --- Task Messages ---
    
    /// Submit a new task
    SubmitTask(TaskDefinition),
    
    /// Task has been scheduled
    TaskScheduled {
        task_id: String,
        agent_id: String,
    },
    
    /// Execute a task (sent to worker agent)
    ExecuteTask(TaskDefinition),
    
    /// Task result
    TaskResult(TaskResult),
    
    /// Task failed
    TaskFailed {
        task_id: String,
        error: String,
    },
    
    /// Cancel a task
    CancelTask {
        task_id: String,
        reason: String,
    },
    
    // --- Agent Lifecycle Messages ---
    
    /// Register a new agent
    RegisterAgent(AgentDefinition),
    
    /// Agent is ready
    AgentReady {
        agent_id: String,
    },
    
    /// Agent is busy
    AgentBusy {
        agent_id: String,
        task_count: u32,
    },
    
    /// Agent is idle
    AgentIdle {
        agent_id: String,
    },
    
    /// Deregister an agent
    DeregisterAgent {
        agent_id: String,
        reason: String,
    },
    
    /// Heartbeat
    Heartbeat {
        agent_id: String,
        timestamp: i64,
    },
    
    // --- Flake/Nix Messages (argunix-inspired) ---
    
    /// Analyze a Nix flake
    AnalyzeFlake {
        flake_url: String,
        flake_ref: Option<String>,
        task_id: String,
    },
    
    /// Flake analysis complete
    FlakeAnalysisComplete {
        task_id: String,
        flake_url: String,
        outputs: Vec<NixOutput>,
        dependencies: Vec<Dependency>,
    },
    
    /// Evaluate a flake
    EvaluateFlake {
        flake_url: String,
        flake_ref: String,
        system: String,
        targets: Vec<String>,
        task_id: String,
    },
    
    /// Build a derivation
    BuildDrv {
        drv_path: String,
        task_id: String,
    },
    
    // --- AI Messages ---
    
    /// Request code review
    RequestCodeReview {
        repo_url: String,
        branch: String,
        changes: String,
        handbook: Option<String>,
        task_id: String,
    },
    
    /// Code review complete
    CodeReviewComplete {
        task_id: String,
        review: AIReview,
    },
    
    // --- Storage Messages (Mœ-inspired) ---
    
    /// Store an object
    StoreObject {
        data: Vec<u8>,
        content_type: String,
        metadata: HashMap<String, String>,
        task_id: String,
    },
    
    /// Object stored
    ObjectStored {
        hash: String,
        size: u64,
        storage_location: String,
        task_id: String,
    },
    
    /// Load an object
    LoadObject {
        hash: String,
        task_id: String,
    },
    
    /// Object loaded
    ObjectLoaded {
        hash: String,
        data: Vec<u8>,
        task_id: String,
    },
    
    /// Sync to next generation
    NextGeneration {
        from: u64,
        to: u64,
    },
    
    GenerationSynced {
        from: u64,
        to: u64,
        objects_count: u64,
    },
    
    // --- Trust & Identity Messages (Mœ-inspired) ---
    
    /// Verify identity
    VerifyIdentity {
        public_key: String,
        signature: String,
        challenge: String,
    },
    
    /// Identity verified
    IdentityVerified {
        fingerprint: String,
        trusted: bool,
    },
    
    /// Trust identity
    TrustIdentity {
        fingerprint: String,
        identity: AgentDefinition,
    },
    
    /// Identity trusted
    IdentityTrusted {
        fingerprint: String,
    },
    
    /// Revoke trust
    RevokeTrust {
        fingerprint: String,
        reason: String,
    },
    
    /// Trust revoked
    TrustRevoked {
        fingerprint: String,
    },
    
    // --- Query Messages ---
    
    /// Query knowledge graph
    QueryKnowledge {
        query: String,
        task_id: String,
    },
    
    /// Query result
    QueryResult {
        results: serde_json::Value,
        task_id: String,
    },
    
    // --- System Messages ---
    
    /// Shutdown request
    Shutdown {
        graceful: bool,
    },
    
    /// Status request
    StatusRequest {
        agent_id: Option<String>,
    },
    
    /// Status response
    StatusResponse {
        agents: Vec<AgentDefinition>,
        tasks: Vec<TaskDefinition>,
        system_health: SystemHealth,
    },
    
    /// Log message
    Log {
        level: String,
        message: String,
        agent_id: Option<String>,
        task_id: Option<String>,
    },
}

// Supporting types

/// Nix flake output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NixOutput {
    pub name: String,
    pub output_type: String, // package, check, devShell, nixosConfiguration
    pub system: Option<String>,
    pub drv_path: Option<String>,
    pub description: Option<String>,
}

/// Dependency between flake outputs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub from: String,
    pub to: String,
    pub relationship: String, // dependsOn, extends, etc.
}

/// AI code review result
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct AIReview {
    pub approved: bool,
    pub score: Option<f32>, // 0.0 - 1.0
    pub findings: Vec<AIFinding>,
    pub suggestions: Vec<String>,
    pub summary: Option<String>,
}

/// AI finding
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct AIFinding {
    pub severity: String, // critical, high, medium, low, info
    pub category: String, // security, performance, style, correctness
    pub description: String,
    pub location: Option<AILocation>,
    pub fix_suggestion: Option<String>,
}

/// Code location
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
#[builder(default)]
pub struct AILocation {
    pub file: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub code: Option<String>,
}

/// System health status
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemHealth {
    pub overall_status: String, // healthy, degraded, unhealthy
    pub agents_healthy: u32,
    pub agents_total: u32,
    pub tasks_pending: u32,
    pub tasks_running: u32,
    pub tasks_completed: u64,
    pub storage_available: u64,
    pub storage_used: u64,
}
EOF

# agentflow-core/src/state.rs
cat > agentflow-core/src/state.rs << 'EOF'
use crate::task::{TaskDefinition, TaskFilter, TaskStatus, TaskUpdate};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// In-memory task store for development
#[derive(Debug, Clone, Default)]
pub struct MemoryTaskStore {
    tasks: Arc<parking_lot::RwLock<HashMap<String, TaskDefinition>>>,
}

#[async_trait]
impl TaskStore for MemoryTaskStore {
    async fn create_task(&self, task: &TaskDefinition) -> crate::Result<TaskDefinition> {
        let mut tasks = self.tasks.write();
        tasks.insert(task.id.clone(), task.clone());
        Ok(task.clone())
    }
    
    async fn get_task(&self, id: &str) -> crate::Result<Option<TaskDefinition>> {
        let tasks = self.tasks.read();
        Ok(tasks.get(id).cloned())
    }
    
    async fn update_task(&self, id: &str, update: TaskUpdate) -> crate::Result<TaskDefinition> {
        let mut tasks = self.tasks.write();
        if let Some(task) = tasks.get_mut(id) {
            if let Some(status) = update.status {
                task.status = status;
            }
            if let Some(priority) = update.priority {
                task.priority = priority;
            }
            if let Some(started_at) = update.started_at {
                task.started_at = Some(started_at);
            }
            if let Some(completed_at) = update.completed_at {
                task.completed_at = Some(completed_at);
            }
            if let Some(metadata) = update.metadata {
                task.metadata.extend(metadata);
            }
            Ok(task.clone())
        } else {
            Err(crate::AgentFlowError::NotFound(id.to_string()))
        }
    }
    
    async fn list_tasks(&self, filter: Option<TaskFilter>) -> crate::Result<Vec<TaskDefinition>> {
        let tasks = self.tasks.read();
        let mut result: Vec<TaskDefinition> = tasks.values().cloned().collect();
        
        if let Some(f) = filter {
            if let Some(statuses) = f.status {
                result.retain(|t| statuses.contains(&t.status));
            }
            if let Some(types) = f.task_type {
                result.retain(|t| types.contains(&t.task_type));
            }
            if let Some(min_priority) = f.priority_min {
                result.retain(|t| t.priority >= min_priority);
            }
            if let Some(max_priority) = f.priority_max {
                result.retain(|t| t.priority <= max_priority);
            }
            if let Some(flake_url) = &f.flake_url {
                result.retain(|t| t.flake_url.as_deref() == Some(flake_url));
            }
            if let Some(limit) = f.limit {
                result.truncate(limit);
            }
            if let Some(offset) = f.offset {
                if offset < result.len() {
                    result.drain(..offset);
                } else {
                    result.clear();
                }
            }
        }
        
        Ok(result)
    }
    
    async fn delete_task(&self, id: &str) -> crate::Result<()> {
        let mut tasks = self.tasks.write();
        tasks.remove(id);
        Ok(())
    }
}

/// In-memory agent store
#[derive(Debug, Clone, Default)]
pub struct MemoryAgentStore {
    agents: Arc<parking_lot::RwLock<HashMap<String, AgentDefinition>>>,
}

#[async_trait]
impl StateStore for MemoryAgentStore {
    async fn get_agent(&self, id: &str) -> crate::Result<Option<AgentDefinition>> {
        let agents = self.agents.read();
        Ok(agents.get(id).cloned())
    }
    
    async fn register_agent(&self, agent: &AgentDefinition) -> crate::Result<()> {
        let mut agents = self.agents.write();
        agents.insert(agent.id.clone(), agent.clone());
        Ok(())
    }
    
    async fn dereister_agent(&self, id: &str) -> crate::Result<()> {
        let mut agents = self.agents.write();
        agents.remove(id);
        Ok(())
    }
    
    async fn list_agents(&self) -> crate::Result<Vec<AgentDefinition>> {
        let agents = self.agents.read();
        Ok(agents.values().cloned().collect())
    }
}

/// System state
#[derive(Debug, Clone, Default)]
pub struct SystemState {
    pub task_store: Arc<dyn TaskStore + Send + Sync>,
    pub agent_store: Arc<dyn StateStore + Send + Sync>,
}

impl SystemState {
    pub fn new() -> Self {
        Self {
            task_store: Arc::new(MemoryTaskStore::default()),
            agent_store: Arc::new(MemoryAgentStore::default()),
        }
    }
}
EOF

echo "✅ Core types created"
