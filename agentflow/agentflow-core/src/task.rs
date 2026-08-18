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
    
    // Storage/Cache tasks
    #[strum(serialize = "store-object")]
    StoreObject,
    #[strum(serialize = "load-object")]
    LoadObject,
    #[strum(serialize = "cache-check")]
    CacheCheck,
    #[strum(serialize = "cache-upload")]
    CacheUpload,
    #[strum(serialize = "cache-cleanup")]
    CacheCleanup,
    
    // Git tasks
    #[strum(serialize = "sync-repository")]
    SyncRepository,
    #[strum(serialize = "poll-repository")]
    PollRepository,
    #[strum(serialize = "setup-repository")]
    SetupRepository,
    #[strum(serialize = "poll-all-repositories")]
    PollAllRepositories,
    #[strum(serialize = "webhook-received")]
    WebhookReceived,
    #[strum(serialize = "get-repository-status")]
    GetRepositoryStatus,
    
    // QEMU tasks
    #[strum(serialize = "provision-vm")]
    ProvisionVM,
    #[strum(serialize = "destroy-vm")]
    DestroyVM,
    #[strum(serialize = "run-tests")]
    RunTests,
    
    // Generic
    #[strum(serialize = "custom-command")]
    CustomCommand,
    #[strum(serialize = "multi-task")]
    MultiTask,
}

/// Task status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, Display, EnumString, Default)]
pub enum TaskStatus {
    #[strum(serialize = "pending")]
    #[default]
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
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder)]
#[builder(default)]
pub struct TaskDefinition {
    /// Unique task identifier
    pub id: String,
    
    /// Task type
    pub task_type: TaskType,
    
    /// Task status
    #[builder(setter(skip))]
    pub status: TaskStatus,
    
    /// Priority (0-100)
    #[builder(default = "50")]
    pub priority: Priority,
    
    /// Creation timestamp
    #[builder(setter(skip))]
    pub created_at: DateTime<Utc>,
    
    /// Start timestamp
    #[builder(setter(skip))]
    pub started_at: Option<DateTime<Utc>>,
    
    /// Completion timestamp
    #[builder(setter(skip))]
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
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder, Default)]
#[builder(default)]
pub struct ResourceRequirements {
    pub cpu: Option<f32>,
    pub memory: Option<u64>,
    pub storage: Option<u64>,
    pub gpu: Option<u32>,
    pub gpu_memory: Option<u64>,
}

/// Task constraints
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder, Default)]
#[builder(default)]
pub struct TaskConstraints {
    pub node_labels: Option<HashMap<String, String>>,
    pub affinity: Option<AffinityRules>,
    pub timeout_seconds: Option<u64>,
    pub retry_policy: Option<RetryPolicy>,
    pub data_locality: Option<DataLocality>,
    pub compliance_tags: Option<Vec<String>>,
}

/// Affinity rules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AffinityType {
    Required,
    Preferred,
    AntiAffinity,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AffinityRules {
    pub rules: Vec<AffinityRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffinityRule {
    pub affinity_type: AffinityType,
    pub key: String,
    pub values: Option<Vec<String>>,
}

/// Retry policy
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder, Default)]
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
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder, Default)]
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
#[derive(Debug, Clone, Serialize, Deserialize, derive_builder::Builder, Default)]
#[builder(default)]
pub struct Artifact {
    pub name: String,
    pub path: String,
    pub hash: Option<String>,
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub storage_location: Option<String>,
}
