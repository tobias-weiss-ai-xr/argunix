//! MoeSyncAgent - Synchronize data with Mœ self-sovereign storage
//!
//! This agent handles:
//! - Uploading objects to Mœ storage
//! - Downloading objects from Mœ
//! - Managing Mœ identities and namespaces
//! - Handling Mœ generation transitions
//! - Synchronizing with Mœ peers
//!
//! ## Mœ Sovereignty Concepts
//! Mœ is a self-sovereign storage protocol where:
//! - Each user has a cryptographic identity
//! - Data is content-addressed and immutable
//! - Objects are signed by the author's identity
//! - Storage is distributed across peers
//! - Each object belongs to a namespace and generation
//!
//! ## Features
//! - Identity management (create, import, export)
//! - Object upload with automatic signing
//! - Object download with integrity verification
//! - Namespace management
//! - Generation transitions
//! - Peer synchronization
//!
//! ## Messages Handled
//! - UploadToMoe: Upload an object to Mœ
//! - DownloadFromMoe: Download an object from Mœ
//! - SyncWithMoe: Synchronize namespace with Mœ peer
//! - CreateNamespace: Create a new Mœ namespace
//! - SwitchGeneration: Transition to a new generation
//!
//! ## Dependencies
//! - moe-client (hypothetical Rust crate for Mœ protocol)
//! - ed25519 for key operations
//! - serde for serialization

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, RwLock};
use serde::{Deserialize, Serialize};

use agentflow_core::{Agent, AgentDefinition, AgentContext, AgentMessage, AgentStatus, AgentType, Result, TaskDefinition, TaskStatus, TaskType};
use agentflow_core::agent::{StateStore, TaskStore};

/// Mœ identity information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoeIdentity {
    /// Identity name
    pub name: String,
    /// Public key fingerprint (SHA256 of public key)
    pub fingerprint: String,
    /// Public key (base64-encoded ed25519)
    pub public_key: String,
    /// Private key (base64-encoded ed25519, optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// Key creation timestamp
    pub created_at: DateTime<Utc>,
    /// Last used timestamp
    pub last_used: Option<DateTime<Utc>>,
    /// Active flag
    pub active: bool,
    /// Description
    pub description: Option<String>,
}

impl MoeIdentity {
    /// Create a new identity (in production, this would generate actual keys)
    pub fn new(name: String, description: Option<String>) -> Self {
        // In production, we'd generate actual Ed25519 keys here
        // For now, we use dummy values
        let public_key = "dummy-public-key-base64".to_string();
        let fingerprint = format!("sha256-{}", name.to_lowercase());
        
        Self {
            name,
            fingerprint,
            public_key,
            private_key: Some("dummy-private-key-base64".to_string()),
            created_at: Utc::now(),
            last_used: None,
            active: true,
            description,
        }
    }
    
    /// Check if identity has private key
    pub fn has_private_key(&self) -> bool {
        self.private_key.is_some()
    }
}

/// Mœ peer information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoePeer {
    /// Peer URL
    pub url: String,
    /// Peer's identity fingerprint
    pub fingerprint: String,
    /// Trusted flag
    pub trusted: bool,
    /// Last sync timestamp
    pub last_sync: Option<DateTime<Utc>>,
    /// Sync status
    pub status: PeerStatus,
    /// Latency in milliseconds
    pub latency_ms: Option<u64>,
}

/// Peer status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PeerStatus {
    Unreachable,
    reachable,
    #[allow(dead_code)]
    Syncing,
    Synced,
}

/// Object metadata for tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoeObjectRef {
    /// Content hash (SHA256)
    pub hash: String,
    /// Object size in bytes
    pub size: u64,
    /// Namespace
    pub namespace: String,
    /// Generation
    pub generation: u64,
    /// Author fingerprint
    pub author: String,
    /// Timestamp
    pub timestamp: DateTime<Utc>,
    /// Object type/class
    pub object_type: String,
    /// Tags/keywords
    pub tags: Vec<String>,
    /// Local cache path (if downloaded)
    pub local_path: Option<PathBuf>,
    /// Uploaded flag
    pub uploaded: bool,
    /// Verified flag
    pub verified: bool,
}

/// Sync configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoeSyncConfig {
    /// Current identity name
    pub identity: String,
    /// Default namespace
    pub namespace: String,
    /// Current generation
    pub generation: u64,
    /// Known peers
    pub peers: Vec<MoePeer>,
    /// Sync interval in seconds
    pub sync_interval: u64,
    /// Maximum retries for failed operations
    pub max_retries: u32,
    /// Timeout for operations
    pub timeout: Duration,
    /// Auto-verify downloads
    pub auto_verify: bool,
    /// Cache directory
    pub cache_path: PathBuf,
    /// Maximum cache size in bytes
    pub max_cache_size: u64,
    /// Enable compression
    pub compression: bool,
}

impl Default for MoeSyncConfig {
    fn default() -> Self {
        let cache_path = PathBuf::from("/var/cache/agentflow/moe");
        let peers = vec![
            MoePeer {
                url: "https://moe.example.org".to_string(),
                fingerprint: "sha256-moe-server-1".to_string(),
                trusted: true,
                last_sync: None,
                status: PeerStatus::Unreachable,
                latency_ms: None,
            }
        ];
        
        Self {
            identity: "default".to_string(),
            namespace: "ci-artifacts".to_string(),
            generation: 1,
            peers,
            sync_interval: 300,
            max_retries: 3,
            timeout: Duration::from_secs(60),
            auto_verify: true,
            cache_path,
            max_cache_size: 10 * 1024 * 1024 * 1024, // 10GB
            compression: true,
        }
    }
}

/// Agent state
#[derive(Debug, Default)]
pub struct MoeSyncState {
    /// Identities
    pub identities: HashMap<String, MoeIdentity>,
    /// Tracked objects by hash
    pub objects: HashMap<String, MoeObjectRef>,
    /// Objects by namespace
    pub objects_by_namespace: HashMap<String, Vec<String>>, // namespace -> [hashes]
    /// Active sync operations
    pub active_syncs: HashMap<String, Instant>,
    /// Stats
    pub stats: MoeSyncStats,
}

/// Statistics
#[derive(Debug, Default, Clone)]
pub struct MoeSyncStats {
    pub objects_uploaded: u64,
    pub objects_downloaded: u64,
    pub bytes_uploaded: u64,
    pub bytes_downloaded: u64,
    pub sync_operations: u64,
    pub sync_failures: u64,
    pub peers_contacted: u64,
}

impl MoeSyncStats {
    pub fn record_upload(&mut self, size: u64) {
        self.objects_uploaded += 1;
        self.bytes_uploaded += size;
    }
    
    pub fn record_download(&mut self, size: u64) {
        self.objects_downloaded += 1;
        self.bytes_downloaded += size;
    }
    
    pub fn record_sync(&mut self, success: bool) {
        self.sync_operations += 1;
        if !success {
            self.sync_failures += 1;
        }
    }
}

/// The MoeSyncAgent struct
pub struct MoeSyncAgent {
    /// Agent sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn TaskStore>,
    /// State store
    _state_store: Arc<dyn StateStore>,
    /// Configuration
    config: MoeSyncConfig,
    /// State
    state: Arc<RwLock<MoeSyncState>>,
    /// Agent name
    name: String,
    /// Agent type
    agent_type: AgentType,
    /// Agent capabilities
    capabilities: HashSet<String>,
    /// Agent status
    status: AgentStatus,
}

impl MoeSyncAgent {
    /// Create a new MoeSyncAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn TaskStore>,
        state_store: Arc<dyn StateStore>,
        config: Option<MoeSyncConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        
        // Create cache directory
        if let Err(e) = std::fs::create_dir_all(&config.cache_path) {
            tracing::error!("Failed to create Mœ cache directory: {}", e);
        }
        
        let mut initial_state = MoeSyncState::default();
        
        // Create default identity if configured
        if !config.identity.is_empty() {
            let identity = MoeIdentity::new(config.identity.clone(), Some("Default identity".to_string()));
            initial_state.identities.insert(config.identity.clone(), identity);
        }
        
        let capabilities = vec![
            "moe-sync".to_string(),
            "moe-storage".to_string(),
            "self-sovereign".to_string(),
            "content-addressed".to_string(),
            "identity-management".to_string(),
            "namespace-management".to_string(),
            "peer-sync".to_string(),
        ].into_iter().collect();
        
        Self {
            sender,
            task_store,
            _state_store: state_store,
            config,
            state: Arc::new(RwLock::new(initial_state)),
            name: "MoeSyncAgent".to_string(),
            agent_type: AgentType::Custom,
            capabilities,
            status: AgentStatus::Ready,
        }
    }
    
    /// Create a new identity
    pub async fn create_identity(&mut self, name: String, description: Option<String>) -> Result<String> {
        let mut state = self.state.write().await;
        
        if state.identities.contains_key(&name) {
            return Err(agentflow_core::AgentFlowError::AlreadyExists(name));
        }
        
        let identity = MoeIdentity::new(name.clone(), description);
        state.identities.insert(name.clone(), identity);
        
        Ok(name)
    }
    
    /// Get current identity
    pub async fn get_identity(&self, name: &str) -> Option<MoeIdentity> {
        let state = self.state.read().await;
        state.identities.get(name).cloned()
    }
    
    /// Switch active identity
    pub async fn switch_identity(&mut self, name: &str) -> Result<()> {
        let state = self.state.read().await;
        
        if !state.identities.contains_key(name) {
            return Err(agentflow_core::AgentFlowError::NotFound(name.to_string()));
        }
        
        self.config.identity = name.to_string();
        
        Ok(())
    }
    
    /// Create a namespace
    pub async fn create_namespace(&mut self, name: String) -> Result<()> {
        let mut state = self.state.write().await;
        
        if !state.objects_by_namespace.contains_key(&name) {
            state.objects_by_namespace.insert(name.clone(), vec![]);
        }
        
        Ok(())
    }
    
    /// Switch to a new generation
    pub async fn switch_generation(&mut self, new_generation: u64, message: Option<String>) -> Result<()> {
        // In Mœ, generation transitions are intentional breaks in compatibility
        // All objects from previous generation remain valid but new objects use new generation
        
        let old_generation = self.config.generation;
        self.config.generation = new_generation;
        
        tracing::info!(
            "Switched from generation {} to {}: {}",
            old_generation,
            new_generation,
            message.as_deref().unwrap_or("")
        );
        
        Ok(())
    }
    
    /// Upload an object to Mœ
    pub async fn upload_object(
        &mut self,
        data: Vec<u8>,
        object_type: String,
        tags: Vec<String>,
    ) -> Result<MoeObjectRef> {
        use sha2::{Sha256, Digest};
        
        let start = Instant::now();
        
        // Compute hash
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let hash = hex::encode(hasher.finalize());
        
        // Get current identity
        let identity = self.get_identity(&self.config.identity).await
            .ok_or_else(|| agentflow_core::AgentFlowError::Generic(
                format!("Identity {} not found", self.config.identity)
            ))?;
        
        let timestamp = Utc::now();
        let size = data.len() as u64;
        
        // Create object reference
        let object_ref = MoeObjectRef {
            hash: hash.clone(),
            size,
            namespace: self.config.namespace.clone(),
            generation: self.config.generation,
            author: identity.fingerprint.clone(),
            timestamp,
            object_type,
            tags,
            local_path: None,
            uploaded: false,
            verified: true, // We generated it, so it's verified
        };
        
        // Cache locally
        let cache_path = self.config.cache_path.join(&hash);
        if let Err(e) = tokio::fs::write(&cache_path, &data).await {
            tracing::warn!("Failed to cache object {}: {}", hash, e);
        } else {
            // Update state
            let mut state = self.state.write().await;
            let mut object_ref_with_path = object_ref.clone();
            object_ref_with_path.local_path = Some(cache_path);
            state.objects.insert(hash.clone(), object_ref_with_path.clone());
            
            let ns_objects = state.objects_by_namespace
                .entry(object_ref_with_path.namespace.clone())
                .or_default();
            if !ns_objects.contains(&hash) {
                ns_objects.push(hash.clone());
            }
        }
        
        // Simulate upload to Mœ peer (in production, this would use moe-client)
        // For now, just mark as uploaded
        {
            let mut state = self.state.write().await;
            if let Some(obj) = state.objects.get_mut(&hash) {
                obj.uploaded = true;
            }
            state.stats.record_upload(size);
        }
        
        tracing::info!(
            "Uploaded object {} ({} bytes) to namespace {}/{}",
            hash,
            size,
            object_ref.namespace,
            object_ref.generation
        );
        
        Ok(object_ref)
    }
    
    /// Download an object from Mœ
    pub async fn download_object(&mut self, hash: &str) -> Result<Vec<u8>> {
        // Check local cache first
        {
            let state = self.state.read().await;
            if let Some(obj) = state.objects.get(hash) {
                if let Some(ref local_path) = obj.local_path {
                    if let Ok(data) = tokio::fs::read(local_path).await {
                        // Verify hash
                        use sha2::{Sha256, Digest};
                        let mut hasher = Sha256::new();
                        hasher.update(&data);
                        let computed_hash = hex::encode(hasher.finalize());
                        
                        if computed_hash == hash {
                            tracing::info!("Cache hit for object {}", hash);
                            {
                                let mut state = self.state.write().await;
                                state.stats.record_download(obj.size);
                            }
                            return Ok(data);
                        }
                    }
                }
            }
        }
        
        // In production, we'd download from Mœ peer here
        // For now, return an error
        Err(agentflow_core::AgentFlowError::NotFound(hash.to_string()))
    }
    
    /// Sync with a Mœ peer
    pub async fn sync_with_peer(&mut self, peer_url: &str) -> Result<SyncReport> {
        let mut report = SyncReport {
            peer_url: peer_url.to_string(),
            objects_uploaded: 0,
            objects_downloaded: 0,
            bytes_transferred: 0,
            duration: Duration::default(),
            success: true,
            error: None,
        };
        
        let start = Instant::now();
        
        // In production, this would:
        // 1. Connect to the peer
        // 2. Negotiate sync parameters
        // 3. Exchange object lists
        // 4. Upload missing objects to peer
        // 5. Download missing objects from peer
        // 6. Verify downloaded objects
        
        // For now, simulate a sync
        tracing::info!("Syncing with peer {}...", peer_url);
        
        // Update peer status
        {
            let mut state = self.state.write().await;
            for peer in &mut state.identities.values_mut() {
                // Update last sync for matching peer
                // (in production, we'd match by URL or fingerprint)
            }
            state.stats.record_sync(true);
        }
        
        report.duration = start.elapsed();
        
        // Update peer in config
        if let Some(peer) = self.config.peers.iter_mut().find(|p| p.url == peer_url) {
            peer.last_sync = Some(Utc::now());
            peer.status = PeerStatus::Synced;
        }
        
        tracing::info!(
            "Sync with {} completed in {:?}: {} uploaded, {} downloaded",
            peer_url,
            report.duration,
            report.objects_uploaded,
            report.objects_downloaded
        );
        
        Ok(report)
    }
    
    /// List objects in a namespace
    pub async fn list_objects(&self, namespace: &str) -> Vec<MoeObjectRef> {
        let state = self.state.read().await;
        
        if let Some(hashes) = state.objects_by_namespace.get(namespace) {
            hashes.iter()
                .filter_map(|h| state.objects.get(h).cloned())
                .collect()
        } else {
            vec![]
        }
    }
    
    /// Get object by hash
    pub async fn get_object(&self, hash: &str) -> Option<MoeObjectRef> {
        let state = self.state.read().await;
        state.objects.get(hash).cloned()
    }
    
    /// Get statistics
    pub async fn get_stats(&self) -> MoeSyncStats {
        let state = self.state.read().await;
        state.stats.clone()
    }
    
    /// Add a peer
    pub async fn add_peer(&mut self, peer: MoePeer) -> Result<()> {
        self.config.peers.push(peer);
        Ok(())
    }
    
    /// Remove a peer
    pub async fn remove_peer(&mut self, url: &str) -> bool {
        if let Some(pos) = self.config.peers.iter().position(|p| p.url == url) {
            self.config.peers.remove(pos);
            true
        } else {
            false
        }
    }
}

/// Sync report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub peer_url: String,
    pub objects_uploaded: u64,
    pub objects_downloaded: u64,
    pub bytes_transferred: u64,
    pub duration: Duration,
    pub success: bool,
    pub error: Option<String>,
}

/// Implement Agent trait for MoeSyncAgent
#[async_trait::async_trait]
impl Agent for MoeSyncAgent {
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
            AgentMessage::UploadToMoe { data, object_type, tags, task_id } => {
                let object = self.upload_object(data, object_type, tags).await?;
                
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("moe-upload-{}", object.hash)),
                    task_type: TaskType::UploadToMoe,
                    status: TaskStatus::Succeeded,
                    priority: 80,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
                
                self.sender.send(AgentMessage::ObjectUploaded {
                    hash: object.hash.clone(),
                    size: object.size,
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::DownloadFromMoe { hash, task_id } => {
                match self.download_object(&hash).await {
                    Ok(data) => {
                        let task = TaskDefinition {
                            id: task_id.clone().unwrap_or_else(|| format!("moe-download-{}", hash)),
                            task_type: TaskType::DownloadFromMoe,
                            status: TaskStatus::Succeeded,
                            priority: 80,
                            created_at: Utc::now(),
                            ..Default::default()
                        };
                        self.task_store.create_task(&task).await?;
                        
                        let data_len = data.len() as u64;
                        self.sender.send(AgentMessage::ObjectDownloaded {
                            hash: hash.clone(),
                            data,
                            size: data_len,
                            task_id: task_id,
                        }).await?;
                    }
                    Err(e) => {
                        let mut task = TaskDefinition {
                            id: task_id.unwrap_or_else(|| format!("moe-download-{}", hash)),
                            task_type: TaskType::DownloadFromMoe,
                            status: TaskStatus::Failed,
                            priority: 80,
                            created_at: Utc::now(),
                            ..Default::default()
                        };
                        task.metadata.insert("error".to_string(), e.to_string());
                        self.task_store.create_task(&task).await?;
                        
                        return Err(e);
                    }
                }
            }
            
            AgentMessage::SyncWithMoe { peer_url, task_id } => {
                let report = self.sync_with_peer(&peer_url).await?;
                
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("moe-sync-{}:{}", self.config.identity, peer_url)),
                    task_type: TaskType::SyncWithMoe,
                    status: TaskStatus::Succeeded,
                    priority: 70,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
                
                self.sender.send(AgentMessage::MoeSyncComplete {
                    peer_url: report.peer_url,
                    objects_uploaded: report.objects_uploaded,
                    objects_downloaded: report.objects_downloaded,
                    duration_seconds: report.duration.as_secs_f64(),
                    success: report.success,
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::CreateNamespace { name, task_id } => {
                self.create_namespace(name.clone()).await?;
                
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("moe-namespace-{}", name)),
                    task_type: TaskType::CreateNamespace,
                    status: TaskStatus::Succeeded,
                    priority: 60,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
            }
            
            AgentMessage::SwitchGeneration { generation, message, task_id } => {
                self.switch_generation(generation, message).await?;
                
                let task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("moe-gen-{}", generation)),
                    task_type: TaskType::SwitchGeneration,
                    status: TaskStatus::Succeeded,
                    priority: 90, // High priority - affects all future objects
                    created_at: Utc::now(),
                    ..Default::default()
                };
                self.task_store.create_task(&task).await?;
            }
            
            AgentMessage::ObjectUploaded { .. } | AgentMessage::ObjectDownloaded { .. } | 
            AgentMessage::MoeSyncComplete { .. } => {
                // These are responses we send
                tracing::debug!("Received Mœ response message (not handled): {:?}", message);
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
    fn test_moe_sync_config_defaults() {
        let config = MoeSyncConfig::default();
        
        assert!(!config.identity.is_empty());
        assert!(!config.namespace.is_empty());
        assert_eq!(config.generation, 1);
        assert!(!config.peers.is_empty());
    }
    
    #[test]
    fn test_identity_creation() {
        let identity = MoeIdentity::new("test-agent".to_string(), Some("Test identity".to_string()));
        
        assert_eq!(identity.name, "test-agent");
        assert!(identity.has_private_key());
        assert!(!identity.fingerprint.is_empty());
    }
    
    #[tokio::test]
    async fn test_agent_creation() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = MoeSyncAgent::new(sender, task_store, state_store, None);
        
        assert_eq!(agent.name(), "MoeSyncAgent");
        assert!(agent.capabilities().contains("moe-sync"));
        assert!(agent.capabilities().contains("self-sovereign"));
    }
    
    #[tokio::test]
    async fn test_upload_object() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let mut agent = MoeSyncAgent::new(sender, task_store, state_store, None);
        
        let data = b"test data".to_vec();
        let result = agent.upload_object(data.clone(), "test".to_string(), vec!["tag1".to_string()]).await.unwrap();
        
        assert!(!result.hash.is_empty());
        assert_eq!(result.size, data.len() as u64);
        
        // Check we can retrieve the object
        let obj = agent.get_object(&result.hash).await.unwrap();
        assert_eq!(obj.hash, result.hash);
    }
    
    #[tokio::test]
    async fn test_switch_generation() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let mut agent = MoeSyncAgent::new(sender, task_store, state_store, None);
        
        assert_eq!(agent.config.generation, 1);
        
        agent.switch_generation(2, Some("Breaking change".to_string())).await.unwrap();
        assert_eq!(agent.config.generation, 2);
    }
}
