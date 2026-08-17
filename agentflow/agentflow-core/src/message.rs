use crate::{AgentDefinition};
use crate::task::{TaskDefinition, TaskResult};
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
    
    /// Generation synced
    GenerationSynced {
        from: u64,
        to: u64,
        objects_count: u64,
    },
    
    // --- Cache Messages ---
    
    /// Check if object exists in cache
    CheckCache {
        hash: String,
        task_id: String,
    },
    
    /// Cache check result
    CacheCheckResult {
        hash: String,
        exists: bool,
        location: Option<String>,
        size: Option<u64>,
        task_id: String,
    },
    
    /// Upload to cache
    UploadToCache {
        hash: String,
        data: Vec<u8>,
        content_type: String,
        metadata: HashMap<String, String>,
        task_id: String,
    },
    
    /// Cache upload complete
    CacheUploaded {
        hash: String,
        size: u64,
        storage_backend: String,
        task_id: String,
    },
    
    /// Cache statistics
    CacheStats {
        total_objects: u64,
        total_size: u64,
        hit_rate: f32,
        backends: HashMap<String, CacheBackendStats>,
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

/// Cache backend statistics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheBackendStats {
    pub object_count: u64,
    pub total_size: u64,
    pub hit_count: u64,
    pub miss_count: u64,
}

/// AI code review result
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder, Default)]
#[builder(default)]
pub struct AIReview {
    pub approved: bool,
    pub score: Option<f32>, // 0.0 - 1.0
    pub findings: Vec<AIFinding>,
    pub suggestions: Vec<String>,
    pub summary: Option<String>,
}

/// AI finding
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder, Default)]
#[builder(default)]
pub struct AIFinding {
    pub severity: String, // critical, high, medium, low, info
    pub category: String, // security, performance, style, correctness
    pub description: String,
    pub location: Option<AILocation>,
    pub fix_suggestion: Option<String>,
}

/// Code location
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder, Default)]
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
