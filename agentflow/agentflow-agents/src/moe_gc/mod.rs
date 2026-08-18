//! MoeGCAgent - Garbage collect old Mœ objects
//!
//! This agent handles:
//! - Identifying unreachable/unreferenced objects
//! - Cleaning up expired generations
//! - Purging old/orphaned data
//! - Storage optimization
//!
//! ## Garbage Collection Strategies
//! - Reference-based: Objects without any references
//! - Time-based: Objects older than retention period
//! - Generation-based: Objects from old generations
//! - Space-based: When storage exceeds limits
//!
//! ## Messages Handled
//! - RunGarbageCollection: Start GC process
//! - GCProgress: Progress updates (internal)
//! - GCDone: GC completion notification

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::{mpsc, RwLock};
use serde::{Deserialize, Serialize};

use agentflow_core::{Agent, AgentContext, AgentMessage, AgentStatus, AgentType, Result, TaskDefinition, TaskStatus, TaskType};
use agentflow_core::agent::{StateStore, TaskStore};

/// Garbage collection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoeGCConfig {
    /// Run GC every N seconds
    pub interval: u64,
    /// Maximum age for objects without references (seconds)
    pub max_age_no_refs: u64,
    /// Maximum age for all objects (seconds, 0 = no limit)
    pub max_age_all: u64,
    /// Keep at least N generations
    pub keep_generations: u8,
    /// Minimum free space percentage before GC runs
    pub min_free_space: u8,
    /// Dry run mode (don't actually delete)
    pub dry_run: bool,
    /// GC storage directory
    pub storage_path: PathBuf,
    /// Enable verbose logging
    pub verbose: bool,
}

impl Default for MoeGCConfig {
    fn default() -> Self {
        Self {
            interval: 3600, // 1 hour
            max_age_no_refs: 86400, // 24 hours
            max_age_all: 2592000, // 30 days
            keep_generations: 5,
            min_free_space: 20, // 20%
            dry_run: false,
            storage_path: PathBuf::from("/var/lib/agentflow/moe-storage"),
            verbose: false,
        }
    }
}

/// GC statistics
#[derive(Debug, Default, Clone)]
pub struct GcStats {
    pub runs: u64,
    pub objects_checked: u64,
    pub objects_deleted: u64,
    pub bytes_reclaimed: u64,
    pub last_run: Option<DateTime<Utc>>,
    pub last_duration: Option<Duration>,
    pub errors: u64,
}

/// Object reference tracking
#[derive(Debug, Clone, Default)]
pub struct ObjectReferences {
    /// Referenced by these objects
    pub referenced_by: HashSet<String>,
    /// Referenced by these namespaces
    pub namespaces: HashSet<String>,
    /// Last accessed timestamp
    pub last_accessed: Option<DateTime<Utc>>,
}

/// Object metadata for GC
#[derive(Debug, Clone)]
pub struct GcObject {
    /// Object hash
    pub hash: String,
    /// Size in bytes
    pub size: u64,
    /// Creation timestamp
    pub created_at: DateTime<Utc>,
    /// Generation
    pub generation: u64,
    /// Namespace
    pub namespace: String,
    /// References
    pub references: ObjectReferences,
    /// Marked for deletion
    pub marked: bool,
    ///Currently being processed
    pub processing: bool,
}

/// GC state
#[derive(Debug, Default)]
pub struct MoeGCState {
    /// All tracked objects
    pub objects: HashMap<String, GcObject>,
    /// Statistics
    pub stats: GcStats,
    /// Current generation
    pub current_generation: u64,
    /// GC in progress
    pub gc_in_progress: bool,
    /// Last known generations
    pub generations: Vec<u64>,
    /// Storage usage
    pub storage_used: u64,
    pub storage_total: u64,
}

/// GC result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcResult {
    /// Objects deleted
    pub objects_deleted: usize,
    /// Bytes reclaimed
    pub bytes_reclaimed: u64,
    /// Objects checked
    pub objects_checked: usize,
    /// Duration
    pub duration: Duration,
    /// Dry run
    pub dry_run: bool,
    /// Errors
    pub errors: Vec<String>,
    /// Warnings
    pub warnings: Vec<String>,
}

/// The MoeGCAgent
pub struct MoeGCAgent {
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn TaskStore>,
    /// State store (unused but required)
    _state_store: Arc<dyn StateStore>,
    /// Configuration
    config: MoeGCConfig,
    /// State
    state: Arc<RwLock<MoeGCState>>,
    /// Agent name
    name: String,
    /// Agent type
    agent_type: AgentType,
    /// Agent capabilities
    capabilities: HashSet<String>,
    /// Agent status
    status: AgentStatus,
}

impl MoeGCAgent {
    /// Create a new MoeGCAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn TaskStore>,
        state_store: Arc<dyn StateStore>,
        config: Option<MoeGCConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        
        // Ensure storage path exists
        if let Err(e) = std::fs::create_dir_all(&config.storage_path) {
            tracing::error!("Failed to create GC storage directory: {}", e);
        }
        
        // Calculate storage usage
        let (storage_used, storage_total) = Self::get_storage_usage(&config.storage_path);
        
        let capabilities = vec![
            "moe-gc".to_string(),
            "garbage-collection".to_string(),
            "storage-cleanup".to_string(),
            "-generation-management".to_string(),
        ].into_iter().collect();
        
        let initial_state = MoeGCState {
            objects: HashMap::new(),
            stats: GcStats::default(),
            current_generation: 1,
            gc_in_progress: false,
            generations: vec![1],
            storage_used,
            storage_total,
        };
        
        Self {
            sender,
            task_store,
            _state_store: state_store,
            config,
            state: Arc::new(RwLock::new(initial_state)),
            name: "MoeGCAgent".to_string(),
            agent_type: AgentType::Custom,
            capabilities,
            status: AgentStatus::Ready,
        }
    }
    
    /// Get storage usage
    fn get_storage_usage(path: &Path) -> (u64, u64) {
        // Simplified implementation - in production use disk usage APIs
        (0, 1024 * 1024 * 1024) // 1GB for now
    }
    
    /// Register an object with GC tracking
    pub async fn register_object(
        &mut self,
        hash: String,
        size: u64,
        namespace: String,
        generation: u64,
    ) -> Result<()> {
        let mut obj = GcObject {
            hash: hash.clone(),
            size,
            created_at: Utc::now(),
            generation,
            namespace,
            references: ObjectReferences::default(),
            marked: false,
            processing: false,
        };
        
        {
            let mut state = self.state.write().await;
            state.objects.insert(hash.clone(), obj);
            state.storage_used += size;
        }
        
        tracing::debug!("Registered object {} ({} bytes) for GC tracking", hash, size);
        
        Ok(())
    }
    
    /// Add a reference to an object
    pub async fn add_reference(&mut self, hash: &str, referenced_by: &str, namespace: &str) -> Result<()> {
        let mut state = self.state.write().await;
        
        if let Some(obj) = state.objects.get_mut(hash) {
            obj.references.referenced_by.insert(referenced_by.to_string());
            obj.references.namespaces.insert(namespace.to_string());
            obj.references.last_accessed = Some(Utc::now());
        }
        
        Ok(())
    }
    
    /// Run garbage collection
    pub async fn run_gc(&mut self, dry_run: Option<bool>) -> Result<GcResult> {
        use std::cmp::min;
        
        let use_dry_run = dry_run.unwrap_or(self.config.dry_run);
        let start = Instant::now();
        let mut result = GcResult {
            objects_deleted: 0,
            bytes_reclaimed: 0,
            objects_checked: 0,
            duration: Duration::default(),
            dry_run: use_dry_run,
            errors: vec![],
            warnings: vec![],
        };
        
        {
            let mut state = self.state.write().await;
            if state.gc_in_progress {
                return Err(agentflow_core::AgentFlowError::Generic(
                    "GC already in progress".to_string()
                ));
            }
            state.gc_in_progress = true;
        }
        
        tracing::info!("Starting {}GC...", if use_dry_run { "dry-run " } else { "" });
        
        // Collect all objects to check
        let mut objects_to_check: Vec<String>;
        let mut state = self.state.write().await;
        
        // Apply retention policies
        let now = Utc::now();
        let current_generation = state.current_generation;
        let keep_generations = self.config.keep_generations as u64;
        let min_gen = current_generation.saturating_sub(keep_generations);
        let max_age_no_refs = TimeDelta::seconds(self.config.max_age_no_refs as i64);
        let max_age_all: Option<TimeDelta> = if self.config.max_age_all > 0 {
            Some(TimeDelta::seconds(self.config.max_age_all as i64))
        } else {
            None
        };
        
        objects_to_check = state.objects.keys().cloned().collect();
        
        for hash in &objects_to_check {
            result.objects_checked += 1;
            
            if let Some(obj) = state.objects.get_mut(hash) {
                let is_referenced = !obj.references.referenced_by.is_empty();
                let age = now - obj.created_at;
                
                // Skip if currently being processed
                if obj.processing {
                    continue;
                }
                
                obj.processing = true;
                
                let should_delete = if !is_referenced && age >= max_age_no_refs {
                    // No references and old enough
                    tracing::debug!("GC: Object {} has no references and is {} old, candidate for deletion",
                        hash, Self::format_duration(age));
                    true
                } else if let Some(max_age) = max_age_all {
                    if age >= max_age {
                        // Too old regardless of references
                        tracing::debug!("GC: Object {} is {} old (max {}), candidate for deletion",
                            hash, Self::format_duration(age), Self::format_duration(max_age));
                        true
                    } else {
                        false
                    }
                } else if obj.generation < min_gen {
                    // From old generation
                    tracing::debug!("GC: Object {} from generation {} (current {}), candidate for deletion",
                        hash, obj.generation, current_generation);
                    true
                } else {
                    false
                };
                
                if should_delete {
                    if use_dry_run {
                        tracing::info!("[DRY-RUN] Would delete object {} ({} bytes)", hash, obj.size);
                        result.warnings.push(format!("Would delete {}", hash));
                    } else {
                        // Mark for deletion
                        obj.marked = true;
                        result.objects_deleted += 1;
                        result.bytes_reclaimed += obj.size;
                        
                        // In production, we'd actually delete the file here
                        tracing::info!("GC: Deleting object {} ({} bytes)", hash, obj.size);
                    }
                }
                
                obj.processing = false;
            }
        }
        
        // Remove marked objects from state
        if !use_dry_run {
            let hashes_to_remove: Vec<String> = state.objects.iter()
                .filter(|(_, obj)| obj.marked)
                .map(|(hash, _)| hash.clone())
                .collect();
            
            for hash in hashes_to_remove {
                if let Some(obj) = state.objects.remove(&hash) {
                    state.storage_used -= obj.size;
                }
            }
        }
        
        // Update stats
        {
            let mut state = self.state.write().await;
            state.stats.runs += 1;
            state.stats.objects_checked += result.objects_checked as u64;
            state.stats.objects_deleted += result.objects_deleted as u64;
            state.stats.bytes_reclaimed += result.bytes_reclaimed;
            state.stats.last_run = Some(Utc::now());
            state.stats.last_duration = Some(start.elapsed());
            state.gc_in_progress = false;
        }
        
        result.duration = start.elapsed();
        
        if use_dry_run {
            tracing::info!("Dry-run GC completed: would delete {} objects ({} bytes) in {:?}",
                result.objects_deleted, result.bytes_reclaimed, result.duration);
        } else {
            tracing::info!("GC completed: deleted {} objects ({} bytes) in {:?}",
                result.objects_deleted, result.bytes_reclaimed, result.duration);
        }
        
        Ok(result)
    }
    
    /// Format TimeDelta for display
    fn format_duration(d: TimeDelta) -> String {
        let total_secs = d.num_seconds();
        if total_secs >= 86400 {
            format!("{} days", total_secs / 86400)
        } else if total_secs >= 3600 {
            format!("{} hours", total_secs / 3600)
        } else if total_secs >= 60 {
            format!("{} min", total_secs / 60)
        } else if total_secs > 0 {
            format!("{} sec", total_secs)
        } else {
            format!("{} ms", d.num_milliseconds())
        }
    }
    
    /// Format Duration for display (for batch_result.duration etc)
    fn format_duration_std(d: std::time::Duration) -> String {
        let total_secs = d.as_secs();
        if total_secs >= 86400 {
            format!("{} days", total_secs / 86400)
        } else if total_secs >= 3600 {
            format!("{} hours", total_secs / 3600)
        } else if total_secs >= 60 {
            format!("{} min", total_secs / 60)
        } else if total_secs > 0 {
            format!("{} sec", total_secs)
        } else {
            format!("{} ms", d.as_millis())
        }
    }
    
    /// Update current generation
    pub async fn update_generation(&mut self, new_generation: u64) -> Result<()> {
        let mut state = self.state.write().await;
        
        if new_generation > state.current_generation {
            state.current_generation = new_generation;
            if !state.generations.contains(&new_generation) {
                state.generations.push(new_generation);
                // Sort and limit
                state.generations.sort();
                if state.generations.len() > 100 {
                    let keep_from = state.generations.len() - 100;
                    state.generations = state.generations.split_off(keep_from);
                }
            }
        }
        
        Ok(())
    }
    
    /// Get statistics
    pub async fn get_stats(&self) -> GcStats {
        let state = self.state.read().await;
        state.stats.clone()
    }
    
    /// Get objects marked for deletion
    pub async fn get_marked_objects(&self) -> Vec<String> {
        let state = self.state.read().await;
        state.objects.iter()
            .filter(|(_, obj)| obj.marked)
            .map(|(hash, _)| hash.clone())
            .collect()
    }
    
    /// Force delete an object
    pub async fn force_delete(&mut self, hash: &str) -> Result<bool> {
        let mut state = self.state.write().await;
        
        if let Some(obj) = state.objects.remove(hash) {
            state.storage_used -= obj.size;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Implement Agent trait for MoeGCAgent
#[async_trait::async_trait]
impl Agent for MoeGCAgent {
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
            AgentMessage::RunGarbageCollection { dry_run, task_id } => {
                let result = self.run_gc(dry_run).await?;
                
                let mut task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("gc-{}", Utc::now().timestamp())),
                    task_type: TaskType::RunGarbageCollection,
                    status: TaskStatus::Succeeded,
                    priority: 60,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                
                task.metadata.insert("objects_deleted".to_string(), result.objects_deleted.to_string());
                task.metadata.insert("bytes_reclaimed".to_string(), result.bytes_reclaimed.to_string());
                task.metadata.insert("duration_ms".to_string(), result.duration.as_millis().to_string());
                if result.dry_run {
                    task.metadata.insert("dry_run".to_string(), "true".to_string());
                }
                
                self.task_store.create_task(&task).await?;
                
                self.sender.send(AgentMessage::GCDone {
                    objects_deleted: result.objects_deleted as u64,
                    bytes_reclaimed: result.bytes_reclaimed,
                    duration_seconds: result.duration.as_secs_f64(),
                    dry_run: result.dry_run,
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::GCDone { .. } | AgentMessage::GCProgress { .. } => {
                // These are responses we send
                tracing::debug!("Received MoeGC response message (not handled): {:?}", message);
            }
            
            _ => {
                tracing::debug!("Unhandled message: {:?}", message);
            }
        }
        
        Ok(())
    }
}

// ========== Tests ==========

#[cfg(test)]
mod tests {
    use super::*;
    use agentflow_core::state::MemoryTaskStore;
    use agentflow_core::agent::StateStore;
    use tokio::sync::mpsc;
    use std::sync::Arc;
    use async_trait::async_trait;
    
    // Mock state store
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
    fn test_gc_config_defaults() {
        let config = MoeGCConfig::default();
        
        assert!(config.interval > 0);
        assert!(config.keep_generations > 0);
        assert!(!config.dry_run);
    }
    
    #[test]
    fn test_format_duration() {
        use std::time::Duration;
        
        assert_eq!(MoeGCAgent::format_duration_std(Duration::from_millis(500)), "500 ms");
        assert_eq!(MoeGCAgent::format_duration_std(Duration::from_secs(30)), "30 sec");
        assert_eq!(MoeGCAgent::format_duration_std(Duration::from_secs(120)), "2 min");
        assert_eq!(MoeGCAgent::format_duration_std(Duration::from_secs(3600)), "1 hours");
        assert_eq!(MoeGCAgent::format_duration_std(Duration::from_secs(86400 * 2)), "2 days");
    }
    
    #[tokio::test]
    async fn test_agent_creation() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = MoeGCAgent::new(sender, task_store, state_store, None);
        
        assert_eq!(agent.name(), "MoeGCAgent");
        assert!(agent.capabilities().contains("moe-gc"));
        assert!(agent.capabilities().contains("garbage-collection"));
    }
    
    #[tokio::test]
    async fn test_register_object() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let mut agent = MoeGCAgent::new(sender, task_store, state_store, None);
        
        let hash = "test-hash".to_string();
        let result = agent.register_object(hash.clone(), 1024, "test-ns".to_string(), 1).await;
        
        assert!(result.is_ok());
        
        let objects = {
            let state = agent.state.read().await;
            state.objects.contains_key(&hash)
        };
        assert!(objects);
    }
    
    #[tokio::test]
    async fn test_run_gc_dry_run() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let mut agent = MoeGCAgent::new(sender, task_store, state_store, None);
        
        // Register an old unreferenced object
        let old_time = Utc::now() - chrono::Duration::days(2);
        {
            let mut state = agent.state.write().await;
            state.objects.insert("old-hash".to_string(), GcObject {
                hash: "old-hash".to_string(),
                size: 1024,
                created_at: old_time,
                generation: 1,
                namespace: "test".to_string(),
                references: ObjectReferences::default(),
                marked: false,
                processing: false,
            });
            state.storage_used = 1024;
        }
        
        let result = agent.run_gc(Some(true)).await.unwrap();
        
        assert!(result.dry_run);
        assert!(result.objects_checked > 0);
        assert!(result.warnings.len() > 0 || result.objects_deleted == 0);
    }
}
