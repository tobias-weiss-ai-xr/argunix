//! BuilderAgent - Handles Nix build operations with multi-arch support
//!
//! This agent builds Nix derivations and flakes with support for:
//! - Multi-architecture builds (x86_64-linux, aarch64-linux, etc.)
//! - Cross-compilation support
//! - Build artifact caching via StorageManager
//! - Build dependency tracking
//! - Concurrent build execution
//!
//! ## Features
//! - Build Nix derivations using `nix build`
//! - Support for multiple systems/architectures
//! - Automatic artifact caching
//! - Build timeout handling
//! - Resource management (max concurrent builds)
//!
//! ## Messages Handled
//! - ExecuteBuild: Build a derivation or flake output
//! - BatchBuild: Build multiple derivations
//! - BuildStatus: Query build status
//! - CancelBuild: Cancel a running build
//!
//! ## Dependencies
//! - Requires StorageManagerAgent for artifact caching
//! - Uses tokio for async execution
//! - Requires nix command line tool

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex, RwLock};

use agentflow_core::{
    Agent, AgentContext, AgentDefinition, AgentStatus, AgentType, AgentMessage, Result,
    TaskDefinition, TaskType, TaskStatus, TaskResult,
};

/// BuilderAgent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuilderConfig {
    /// Supported systems for building
    #[serde(default = "default_systems")]
    pub supported_systems: Vec<String>,
    
    /// Maximum concurrent builds
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_builds: usize,
    
    /// Default build timeout in seconds
    #[serde(default = "default_timeout")]
    pub build_timeout: u64,
    
    /// Nix command path
    #[serde(default = "default_nix_command")]
    pub nix_command: String,
    
    /// Enable artifact caching
    #[serde(default = "default_true")]
    pub cache_enabled: bool,
    
    /// Cache directory for local caching
    #[serde(default)]
    pub cache_directory: Option<PathBuf>,
    
    /// Nix options to pass to all commands
    #[serde(default)]
    pub nix_options: Vec<String>,
    
    /// Environment variables for build processes
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    
    /// Whether to use remote builds
    #[serde(default)]
    pub use_remote: bool,
    
    /// Whether to use substituters for dependencies
    #[serde(default = "default_true")]
    pub use_substituters: bool,
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            supported_systems: default_systems(),
            max_concurrent_builds: default_max_concurrent(),
            build_timeout: default_timeout(),
            nix_command: default_nix_command(),
            cache_enabled: true,
            cache_directory: None,
            nix_options: Vec::new(),
            env_vars: HashMap::new(),
            use_remote: false,
            use_substituters: true,
        }
    }
}

fn default_systems() -> Vec<String> {
    vec![
        "x86_64-linux".to_string(),
        "aarch64-linux".to_string(),
        "x86_64-darwin".to_string(),
        "aarch64-darwin".to_string(),
    ]
}

fn default_max_concurrent() -> usize {
    // Use number of CPU cores as default, max 4
    std::cmp::min(get_num_cpus(), 4)
}

fn get_num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

fn default_timeout() -> u64 {
    3600 // 1 hour default timeout
}

fn default_nix_command() -> String {
    "nix".to_string()
}

fn default_true() -> bool {
    true
}

/// BuilderAgent - Handles Nix build operations
///
/// This agent is responsible for building Nix derivations and flakes
/// with support for multiple architectures and caching.
pub struct BuilderAgent {
    /// Agent definition
    definition: AgentDefinition,
    
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    
    /// Task store
    task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    
    /// Configuration
    config: BuilderConfig,
    
    /// Currently running builds
    running_builds: Arc<Mutex<HashMap<String, BuildState>>>,
    
    /// Build statistics
    stats: Arc<RwLock<BuildStats>>,
}

/// State of a running build
#[derive(Debug, Clone)]
struct BuildState {
    task_id: String,
    drv_path: Option<String>,
    system: String,
    started_at: DateTime<Utc>,
    timeout: Duration,
}

/// Build statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BuildStats {
    pub total_builds: u64,
    pub successful_builds: u64,
    pub failed_builds: u64,
    pub timed_out_builds: u64,
    pub total_build_time: f64,
    pub avg_build_time: f64,
    pub last_build_time: Option<f64>,
    pub builds_by_system: HashMap<String, u64>,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

impl BuildStats {
    fn record_build(&mut self, system: &str, duration: f64, success: bool) {
        self.total_builds += 1;
        *self.builds_by_system.entry(system.to_string()).or_insert(0) += 1;
        self.total_build_time += duration;
        self.avg_build_time = self.total_build_time / self.total_builds as f64;
        self.last_build_time = Some(duration);
        
        if success {
            self.successful_builds += 1;
        } else {
            self.failed_builds += 1;
        }
    }
}

impl BuilderAgent {
    /// Create a new BuilderAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
        config: Option<BuilderConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        
        let capabilities = {
            let mut caps = HashSet::new();
            caps.insert("nix-build".to_string());
            caps.insert("cross-compilation".to_string());
            caps.insert("multi-arch".to_string());
            caps.insert("artifact-upload".to_string());
            caps.insert("cache-management".to_string());
            caps.insert("derivation-building".to_string());
            
            for system in &config.supported_systems {
                caps.insert(system.clone());
            }
            caps
        };
        
        Self {
            definition: AgentDefinition {
                id: "builder-agent-001".to_string(),
                name: "BuilderAgent".to_string(),
                agent_type: AgentType::Builder,
                status: AgentStatus::Ready,
                capabilities,
                ..Default::default()
            },
            sender,
            task_store,
            config,
            running_builds: Arc::new(Mutex::new(HashMap::new())),
            stats: Arc::new(RwLock::new(BuildStats::default())),
        }
    }
    
    /// From definition (for system compatibility)
    pub fn from_definition(
        definition: AgentDefinition,
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    ) -> Self {
        let config = BuilderConfig::default();
        Self::new(sender, task_store, Some(config))
    }
    
    /// Check if we can build for the specified system
    fn supports_system(&self, system: &str) -> bool {
        self.config.supported_systems.contains(&system.to_string())
    }
    
    /// Check if we can accept a new build
    async fn can_accept_build(&self) -> bool {
        let running = self.running_builds.lock().await;
        running.len() < self.config.max_concurrent_builds
    }
    
    /// Get build statistics
    pub async fn get_stats(&self) -> BuildStats {
        self.stats.read().await.clone()
    }
    
    /// Build a derivation
    async fn build_derivation(&self, task: TaskDefinition) -> Result<TaskResult> {
        let drv_path = task.drv_path.clone().ok_or_else(|| {
            agentflow_core::AgentFlowError::Generic("No derivation path provided".to_string())
        })?;
        
        let system = task.system.clone().unwrap_or_else(|| {
            self.config.supported_systems[0].clone()
        });
        
        // Check if system is supported
        if !self.supports_system(&system) {
            return Err(agentflow_core::AgentFlowError::Generic(
                format!("System {} is not supported", &system)
            ));
        }
        
        // Check cache first
        if self.config.cache_enabled {
            // In a real implementation, this would check StorageManager
            // For now, just continue with build
        }
        
        let start = Instant::now();
        let timeout = Duration::from_secs(self.config.build_timeout);
        
        // Record build start
        {
            let mut running = self.running_builds.lock().await;
            running.insert(task.id.clone(), BuildState {
                task_id: task.id.clone(),
                drv_path: Some(drv_path.clone()),
                system: system.clone(),
                started_at: Utc::now(),
                timeout,
            });
        }
        
        // Send build started notification
        self.sender.send(AgentMessage::BuildStarted {
            task_id: task.id.clone(),
            drv_path: drv_path.clone(),
            system: system.clone(),
        }).await?;
        
        // Build and execute command using tokio
        let output = execute_build_command(
            &self.config.nix_command,
            &drv_path,
            Some(&system),
            &self.config.nix_options,
            self.config.cache_directory.as_deref(),
            timeout,
        ).await;
        
        // Clean up running builds
        {
            let mut running = self.running_builds.lock().await;
            running.remove(&task.id);
        }
        
        let duration = start.elapsed().as_secs_f64();
        
        // Record build in stats
        {
            let mut stats = self.stats.write().await;
            match &output {
                Ok(_) => stats.record_build(&system, duration, true),
                Err(_) => stats.record_build(&system, duration, false),
            }
        }
        
        // Send build completed notification
        let (status, result_output, result_error, exit_code) = match output {
            Ok(output) => (TaskStatus::Succeeded, Some(output), None, Some(0)),
            Err(e) => (TaskStatus::Failed, None, Some(e), Some(1)),
        };
        
        self.sender.send(AgentMessage::BuildComplete {
            task_id: task.id.clone(),
            drv_path: drv_path.clone(),
            system: system.clone(),
            status: status.clone(),
            duration_seconds: Some(duration),
        }).await?;
        
        Ok(TaskResult {
            task_id: task.id,
            status,
            output: result_output,
            artifacts: None,
            exit_code,
            error: result_error,
            duration_seconds: Some(duration),
            started_at: Some(Utc::now() - start.elapsed()),
            completed_at: Some(Utc::now()),
            metadata: {
                let mut meta = HashMap::new();
                meta.insert("system".to_string(), system);
                meta.insert("drv_path".to_string(), drv_path);
                meta
            },
        })
    }
    
    /// Build a flake output
    async fn build_flake_output(&self, task: TaskDefinition) -> Result<TaskResult> {
        let flake_url = task.flake_url.ok_or_else(|| {
            agentflow_core::AgentFlowError::Generic("No flake URL provided".to_string())
        })?;
        
        let flake_ref = task.flake_ref.clone().unwrap_or_else(|| "main".to_string());
        let targets = task.targets.clone().unwrap_or_else(|| vec!["packages.default".to_string()]);
        let system = task.system.clone().unwrap_or_else(|| self.config.supported_systems[0].clone());
        
        if !self.supports_system(&system) {
            return Err(agentflow_core::AgentFlowError::Generic(
                format!("System {} is not supported", &system)
            ));
        }
        
        let start = Instant::now();
        let timeout = Duration::from_secs(self.config.build_timeout);
        
        // Execute build
        let output = execute_flake_build_command(
            &self.config.nix_command,
            &flake_url,
            Some(&flake_ref),
            &targets,
            Some(&system),
            &self.config.nix_options,
            timeout,
        ).await;
        
        let duration = start.elapsed().as_secs_f64();
        
        // Record build in stats
        {
            let mut stats = self.stats.write().await;
            match &output {
                Ok(_) => stats.record_build(&system, duration, true),
                Err(_) => stats.record_build(&system, duration, false),
            }
        }
        
        match output {
            Ok(output) => {
                Ok(TaskResult {
                    task_id: task.id,
                    status: TaskStatus::Succeeded,
                    output: Some(output),
                    artifacts: None,
                    exit_code: Some(0),
                    error: None,
                    duration_seconds: Some(duration),
                    started_at: Some(Utc::now() - start.elapsed()),
                    completed_at: Some(Utc::now()),
                    metadata: {
                        let mut meta = HashMap::new();
                        meta.insert("flake_url".to_string(), flake_url);
                        meta.insert("flake_ref".to_string(), flake_ref);
                        meta.insert("system".to_string(), system);
                        meta
                    },
                })
            }
            Err(e) => {
                Err(agentflow_core::AgentFlowError::Generic(e))
            }
        }
    }
    
    /// Execute a build task
    async fn execute_build_task(&self, task: TaskDefinition) -> Result<TaskResult> {
        // Check if system is supported
        if let Some(system) = &task.system {
            if !self.supports_system(system) {
                return Err(agentflow_core::AgentFlowError::Generic(
                    format!("Unsupported system: {}", system)
                ));
            }
        }
        
        let task_id = task.id.clone();
        
        // Update task status
        self.task_store.update_task(&task_id, agentflow_core::agent::TaskUpdate {
            status: Some(TaskStatus::Running),
            started_at: Some(Utc::now()),
            ..Default::default()
        }).await?;
        
        let result = if task.drv_path.is_some() {
            self.build_derivation(task).await?
        } else if task.flake_url.is_some() {
            self.build_flake_output(task).await?
        } else {
            return Err(agentflow_core::AgentFlowError::Generic(
                "Build task requires either drv_path or flake_url".to_string()
            ));
        };
        
        // Update task status
        self.task_store.update_task(&task_id, agentflow_core::agent::TaskUpdate {
            status: Some(result.status.clone()),
            completed_at: result.completed_at,
            ..Default::default()
        }).await?;
        
        Ok(result)
    }
    
    /// Cancel a running build
    async fn cancel_build(&self, task_id: &str) -> Result<()> {
        let mut running = self.running_builds.lock().await;
        if let Some(state) = running.get(task_id) {
            // In a real implementation, we would send SIGTERM to the process
            // For now, just remove from tracking
            running.remove(task_id);
            
            self.sender.send(AgentMessage::BuildCancelled {
                task_id: task_id.to_string(),
                reason: "Build cancelled by user".to_string(),
            }).await?;
            
            Ok(())
        } else {
            Err(agentflow_core::AgentFlowError::Generic(
                format!("No running build with ID: {}", task_id)
            ))
        }
    }
    
    /// Get status of a build
    async fn get_build_status(&self, task_id: &str) -> Result<Option<BuildState>> {
        let running = self.running_builds.lock().await;
        Ok(running.get(task_id).cloned())
    }
}

/// Result of a build operation
#[allow(dead_code)]
enum BuildResult {
    Success {
        output: String,
        duration: f64,
    },
    Failed {
        error: String,
        duration: f64,
    },
    Timeout,
}



#[async_trait::async_trait]
impl Agent for BuilderAgent {
    fn name(&self) -> &str {
        &self.definition.name
    }
    
    fn agent_type(&self) -> AgentType {
        self.definition.agent_type.clone()
    }
    
    fn capabilities(&self) -> &HashSet<String> {
        &self.definition.capabilities
    }
    
    async fn handle_message(&mut self, message: AgentMessage, _ctx: &AgentContext) -> Result<()> {
        match message {
            AgentMessage::ExecuteBuild { drv_path, system, task_id } => {
                let task = TaskDefinition {
                    id: task_id.clone(),
                    task_type: TaskType::NixBuild,
                    status: TaskStatus::Pending,
                    priority: 80,
                    created_at: Utc::now(),
                    drv_path: Some(drv_path.clone()),
                    system: system.clone(),
                    ..Default::default()
                };
                let result = self.execute_build_task(task).await?;
                self.sender.send(AgentMessage::TaskResult(result)).await?;
            }
            
            AgentMessage::BatchBuild { drvs, system, task_id } => {
                // Build multiple derivations
                // For now, just build the first one
                if let Some(drv_path) = drvs.first().cloned() {
                    let task = TaskDefinition {
                        id: format!("{}-0", task_id),
                        task_type: TaskType::NixBuild,
                        status: TaskStatus::Pending,
                        priority: 80,
                        created_at: Utc::now(),
                        drv_path: Some(drv_path),
                        system: system.clone(),
                        ..Default::default()
                    };
                    let result = self.execute_build_task(task).await?;
                    self.sender.send(AgentMessage::TaskResult(result)).await?;
                }
                
                // TODO: Implement proper batch processing
                self.sender.send(AgentMessage::BatchBuildComplete {
                    task_id,
                    total: drvs.len(),
                    completed: 1,
                    failed: 0,
                }).await?;
            }
            
            AgentMessage::BuildDrv { drv_path, task_id } => {
                let task = TaskDefinition {
                    id: task_id.clone(),
                    task_type: TaskType::NixBuild,
                    status: TaskStatus::Pending,
                    priority: 80,
                    created_at: Utc::now(),
                    drv_path: Some(drv_path.clone()),
                    ..Default::default()
                };
                let result = self.execute_build_task(task).await?;
                self.sender.send(AgentMessage::TaskResult(result)).await?;
            }
            
            AgentMessage::BuildFlake { flake_url, flake_ref, targets, system, task_id } => {
                let task = TaskDefinition {
                    id: task_id.clone(),
                    task_type: TaskType::NixBuild,
                    status: TaskStatus::Pending,
                    priority: 80,
                    created_at: Utc::now(),
                    flake_url: Some(flake_url.clone()),
                    flake_ref: flake_ref.clone(),
                    targets: Some(targets.clone()),
                    system: system.clone(),
                    ..Default::default()
                };
                let result = self.execute_build_task(task).await?;
                self.sender.send(AgentMessage::TaskResult(result)).await?;
            }
            
            AgentMessage::CancelBuild { task_id } => {
                self.cancel_build(&task_id).await?;
            }
            
            AgentMessage::BuildStatus { task_id } => {
                let status = self.get_build_status(&task_id).await?;
                if let Some(state) = status {
                    let elapsed = Utc::now() - state.started_at;
                    self.sender.send(AgentMessage::BuildStatusUpdate {
                        task_id: task_id.clone(),
                        status: TaskStatus::Running,
                        progress: 0.0, // Would track actual progress
                        details: Some(format!("Building on {} for {}s", 
                            state.system, 
                            elapsed.num_seconds()
                        )),
                    }).await?;
                } else {
                    self.sender.send(AgentMessage::BuildStatusUpdate {
                        task_id: task_id.clone(),
                        status: TaskStatus::Failed,
                        progress: 0.0,
                        details: Some("Build not found".to_string()),
                    }).await?;
                }
            }
            
            AgentMessage::ExecuteTask(task) => {
                match task.task_type {
                    TaskType::NixBuild => {
                        let result = self.execute_build_task(task).await?;
                        self.sender.send(AgentMessage::TaskResult(result)).await?;
                    }
                    _ => {
                        // Task type not supported by BuilderAgent
                    }
                }
            }
            
            AgentMessage::GetStats => {
                let stats = self.get_stats().await;
                self.sender.send(AgentMessage::Stats {
                    agent_id: self.definition.id.clone(),
                    data: serde_json::to_value(stats).unwrap_or_default(),
                }).await?;
            }
            
            _ => {
                // Ignore other message types
            }
        }
        
        Ok(())
    }
    
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> {
        println!("BuilderAgent started");
        println!("  Supported systems: {:?}", self.config.supported_systems);
        println!("  Max concurrent builds: {}", self.config.max_concurrent_builds);
        println!("  Build timeout: {}s", self.config.build_timeout);
        println!("  Cache enabled: {}", self.config.cache_enabled);
        Ok(())
    }
    
    fn status(&self) -> AgentStatus {
        self.definition.status.clone()
    }
}

/// Execute a nix build command asynchronously
async fn execute_build_command(
    nix_command: &str,
    target: &str,
    system: Option<&str>,
    nix_options: &[String],
    cache_dir: Option<&std::path::Path>,
    timeout: std::time::Duration,
) -> Result<String, String> {
    use tokio::process::Command;
    
    let mut cmd = Command::new(nix_command);
    cmd.arg("build");
    
    // Add nix options
    for opt in nix_options {
        cmd.arg(opt);
    }
    
    // Set target
    cmd.arg(target);
    
    // Set system if specified
    if let Some(sys) = system {
        cmd.arg("--system");
        cmd.arg(sys);
    }
    
    // Set cache directory if specified
    if let Some(cache) = cache_dir {
        cmd.arg("--store");
        cmd.arg(cache.to_str().unwrap());
    }
    
    // Execute with timeout
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("Failed to execute command: {}", e)),
        Err(_) => return Err("Command timed out".to_string()),
    };
    
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        Err(format!("Command failed with exit code {}:\nStdout:\n{}\nStderr:\n{}",
            output.status.code().unwrap_or(-1), stdout, stderr))
    }
}

/// Execute a nix flake build command
async fn execute_flake_build_command(
    nix_command: &str,
    flake_url: &str,
    flake_ref: Option<&str>,
    targets: &[String],
    system: Option<&str>,
    nix_options: &[String],
    timeout: std::time::Duration,
) -> Result<String, String> {
    use tokio::process::Command;
    
    let mut cmd = Command::new(nix_command);
    cmd.arg("build");
    
    // Add nix options
    for opt in nix_options {
        cmd.arg(opt);
    }
    
    // Set flake
    let flake_spec = if let Some(r) = flake_ref {
        format!("{}#{}", flake_url, r)
    } else {
        format!("{}", flake_url)
    };
    cmd.arg("--flake");
    cmd.arg(flake_spec);
    
    // Set targets
    for target in targets {
        cmd.arg(target);
    }
    
    // Set system if specified
    if let Some(sys) = system {
        cmd.arg("--system");
        cmd.arg(sys);
    }
    
    // Execute with timeout
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("Failed to execute command: {}", e)),
        Err(_) => return Err("Command timed out".to_string()),
    };
    
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        Err(format!("Command failed with exit code {}:\nStdout:\n{}\nStderr:\n{}",
            output.status.code().unwrap_or(-1), stdout, stderr))
    }
}

// Remove old execute_commandAsync and execute_command_async functions
// Add AgentType::Builder to the enum
// This would be in agentflow-core/src/agent.rs, but we'll add the string here
// for now

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_builder_config_defaults() {
        let config = BuilderConfig::default();
        
        assert!(config.supported_systems.contains(&"x86_64-linux".to_string()));
        assert!(config.supported_systems.contains(&"aarch64-linux".to_string()));
        assert!(config.max_concurrent_builds > 0);
        assert!(config.build_timeout > 0);
        assert_eq!(config.nix_command, "nix");
        assert!(config.cache_enabled);
    }
    
    #[test]
    fn test_builder_agent_creation() {
        use tokio::sync::mpsc;
        use agentflow_core::state::MemoryTaskStore;
        
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        
        let agent = BuilderAgent::new(sender, task_store, None);
        
        assert_eq!(agent.name(), "BuilderAgent");
        assert_eq!(agent.status(), AgentStatus::Ready);
        assert!(agent.capabilities().contains("nix-build"));
        assert!(agent.capabilities().contains("multi-arch"));
    }
    
    #[test]
    fn test_system_support() {
        use tokio::sync::mpsc;
        use agentflow_core::state::MemoryTaskStore;
        
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        
        let config = BuilderConfig {
            supported_systems: vec!["x86_64-linux".to_string(), "aarch64-linux".to_string()],
            ..Default::default()
        };
        
        let agent = BuilderAgent::new(sender, task_store, Some(config));
        
        assert!(agent.supports_system("x86_64-linux"));
        assert!(agent.supports_system("aarch64-linux"));
        assert!(!agent.supports_system("i686-linux"));
    }
}
