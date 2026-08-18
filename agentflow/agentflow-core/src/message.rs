use crate::{AgentDefinition};
use crate::task::{TaskDefinition, TaskResult, TaskStatus};
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
    
    /// Execute a build (generic)
    ExecuteBuild {
        drv_path: String,
        system: Option<String>,
        task_id: String,
    },
    
    /// Build a flake output
    BuildFlake {
        flake_url: String,
        flake_ref: Option<String>,
        targets: Vec<String>,
        system: Option<String>,
        task_id: String,
    },
    
    /// Batch build multiple derivations
    BatchBuild {
        drvs: Vec<String>,
        system: Option<String>,
        task_id: String,
    },
    
    /// Build started notification
    BuildStarted {
        task_id: String,
        drv_path: String,
        system: String,
    },
    
    /// Build complete notification
    BuildComplete {
        task_id: String,
        drv_path: String,
        system: String,
        status: TaskStatus,
        duration_seconds: Option<f64>,
    },
    
    /// Batch build complete
    BatchBuildComplete {
        task_id: String,
        total: usize,
        completed: usize,
        failed: usize,
    },
    
    /// Cancel a build
    CancelBuild {
        task_id: String,
    },
    
    /// Build was cancelled
    BuildCancelled {
        task_id: String,
        reason: String,
    },
    
    /// Request build status
    BuildStatus {
        task_id: String,
    },
    
    /// Build status update
    BuildStatusUpdate {
        task_id: String,
        status: TaskStatus,
        progress: f64,
        details: Option<String>,
    },
    
    // --- Git Messages ---
    
    /// Setup a repository for monitoring
    SetupRepository {
        repo_config: serde_json::Value,
        task_id: Option<String>,
    },
    
    /// Sync a repository
    SyncRepository {
        repo_config: serde_json::Value,
        task_id: Option<String>,
    },
    
    /// Poll a repository for changes
    PollRepository {
        repo_id: String,
        task_id: Option<String>,
    },
    
    /// Poll all repositories
    PollAllRepositories {
        task_id: Option<String>,
    },
    
    /// Webhook received
    WebhookReceived {
        provider: String,
        payload: serde_json::Value,
        signature: Option<String>,
        task_id: Option<String>,
    },
    
    /// Get repository status
    GetRepositoryStatus {
        repo_id: String,
        task_id: Option<String>,
    },
    
    /// Repository status response
    RepositoryStatus {
        repo_id: String,
        status: String,
        last_commit: Option<String>,
        last_sync: Option<String>,
        has_flake: bool,
        healthy: bool,
        error: Option<String>,
    },
    
    // --- QEMU/Testing Messages ---
    
    // Note: These messages use serde_json::Value for config to avoid circular dependencies
    // Actual types are defined in agentflow-agents/src/qemu_test/mod.rs
    
    /// Provision a VM for testing
    ProvisionVM {
        vm_config: serde_json::Value,
        task_id: Option<String>,
    },
    
    /// Destroy a VM
    DestroyVM {
        vm_id: String,
        task_id: Option<String>,
    },
    
    /// VM provisioned successfully
    VMProvisioned {
        vm_id: String,
        ip_address: String,
        ssh_port: u16,
        task_id: Option<String>,
    },
    
    /// VM destroyed
    VMDestroyed {
        vm_id: String,
        task_id: Option<String>,
    },
    
    /// Run tests in a VM
    RunTests {
        test_config: serde_json::Value,
        task_id: Option<String>,
    },
    
    /// Test completed successfully
    TestComplete {
        test_id: String,
        vm_id: Option<String>,
        exit_code: i32,
        output: String,
        duration_seconds: f64,
        task_id: Option<String>,
    },
    
    /// Test failed
    TestFailed {
        test_id: String,
        vm_id: Option<String>,
        error: String,
        exit_code: Option<i32>,
        output: String,
        task_id: Option<String>,
    },
    
    // --- Mœ Messages ---
    
    // Note: These use serde_json::Value for configs to avoid circular dependencies
    // Actual types in agentflow-agents/src/moe_sync/mod.rs
    
    /// Upload an object to Mœ
    UploadToMoe {
        data: Vec<u8>,
        object_type: String,
        tags: Vec<String>,
        task_id: Option<String>,
    },
    
    /// Download an object from Mœ
    DownloadFromMoe {
        hash: String,
        task_id: Option<String>,
    },
    
    /// Object uploaded successfully
    ObjectUploaded {
        hash: String,
        size: u64,
        task_id: Option<String>,
    },
    
    /// Object downloaded successfully
    ObjectDownloaded {
        hash: String,
        data: Vec<u8>,
        size: u64,
        task_id: Option<String>,
    },
    
    /// Sync with a Mœ peer
    SyncWithMoe {
        peer_url: String,
        task_id: Option<String>,
    },
    
    /// Mœ sync completed
    MoeSyncComplete {
        peer_url: String,
        objects_uploaded: u64,
        objects_downloaded: u64,
        duration_seconds: f64,
        success: bool,
        task_id: Option<String>,
    },
    
    /// Create a Mœ namespace
    CreateNamespace {
        name: String,
        task_id: Option<String>,
    },
    
    /// Switch to a new generation
    SwitchGeneration {
        generation: u64,
        message: Option<String>,
        task_id: Option<String>,
    },
    
    // --- MoeVerify Messages ---
    
    /// Verify a Mœ object
    VerifyMoeObject {
        hash: String,
        data: Vec<u8>,
        signer: Option<String>,
        signature: Option<Vec<u8>>,
        task_id: Option<String>,
    },
    
    /// Object verification result
    MoeObjectVerified {
        hash: String,
        valid: bool,
        verification_status: String,
        signer: Option<String>,
        error: Option<String>,
        task_id: Option<String>,
    },
    
    /// Batch verify multiple Mœ objects
    BatchVerifyMoe {
        objects: Vec<VerifyMoeRequest>,
        task_id: Option<String>,
    },
    
    /// Batch verification result
    BatchVerifyMoeComplete {
        results: Vec<MoeVerificationInfo>,
        total: u64,
        valid: u64,
        failed: u64,
        duration_seconds: f64,
        task_id: Option<String>,
    },
    
    /// Verify a Mœ identity
    VerifyMoeIdentity {
        fingerprint: String,
        task_id: Option<String>,
    },
    
    /// Mœ identity verification result
    MoeIdentityVerified {
        fingerprint: String,
        trusted: bool,
        trust_level: Option<u32>,
        task_id: Option<String>,
    },
    
    // Note: Trust/Identity messages already exist below (VerifyIdentity, IdentityVerified, etc.)
    
    // --- MoeGC Messages ---
    
    /// Run garbage collection
    RunGarbageCollection {
        dry_run: Option<bool>,
        task_id: Option<String>,
    },
    
    /// GC progress update
    GCProgress {
        message: String,
        processed: u64,
        total: u64,
        task_id: Option<String>,
    },
    
    /// GC completed
    GCDone {
        objects_deleted: u64,
        bytes_reclaimed: u64,
        duration_seconds: f64,
        dry_run: bool,
        task_id: Option<String>,
    },
    
    // --- GitHub Status Messages ---
    
    /// Post status to GitHub
    PostGitHubStatus {
        owner: String,
        repo: String,
        sha: String,
        state: Option<String>,
        description: Option<String>,
        target_url: Option<String>,
        task_id: Option<String>,
    },
    
    /// GitHub status posted
    GitHubStatusPosted {
        owner: String,
        repo: String,
        sha: String,
        state: String,
        status_url: String,
        task_id: Option<String>,
    },
    
    /// Update GitHub status
    UpdateGitHubStatus {
        owner: String,
        repo: String,
        sha: String,
        state: Option<String>,
        description: Option<String>,
        task_id: Option<String>,
    },
    
    /// GitHub status update failed
    GitHubStatusFailed {
        owner: String,
        repo: String,
        sha: String,
        error: String,
        task_id: Option<String>,
    },
    
    /// Generic GitHub notification
    NotifyGitHub {
        message: String,
        task_id: Option<String>,
    },
    
    // --- Matrix Notifier Messages ---
    
    /// Send notification to Matrix room
    SendMatrixNotification {
        room: String,
        message: String,
        formatted: Option<String>,
        task_id: Option<String>,
    },
    
    /// Matrix notification sent
    MatrixNotificationSent {
        room: String,
        message: String,
        event_id: String,
        task_id: Option<String>,
    },
    
    /// Broadcast message to multiple Matrix rooms
    BroadcastMatrixMessage {
        message: String,
        rooms: Vec<String>,
        task_id: Option<String>,
    },
    
    /// Send file to Matrix
    SendMatrixFile {
        file_name: String,
        content_type: Option<String>,
        data: Vec<u8>,
        room: Option<String>,
        task_id: Option<String>,
    },
    
    /// Matrix file sent
    MatrixFileSent {
        file_name: String,
        content_type: String,
        content_uri: String,
        task_id: Option<String>,
    },
    
    // --- Agent Stats Messages ---
    
    /// Request agent statistics
    GetStats,
    
    /// Agent statistics response
    Stats {
        agent_id: String,
        data: serde_json::Value,
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

// ========== Mœ Verify helper types ==========

/// Request to verify a Mœ object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyMoeRequest {
    pub hash: String,
    pub data: Vec<u8>,
    pub signer: Option<String>,
    pub signature: Option<Vec<u8>>,
}

/// Verification information for Mœ batch results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoeVerificationInfo {
    pub hash: String,
    pub valid: bool,
    pub verification_status: String,
    pub signer: Option<String>,
    pub error: Option<String>,
}
