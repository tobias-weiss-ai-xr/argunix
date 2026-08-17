//! Storage Manager Agent - Manages artifact storage, caching, and retrieval
//!
//! This agent provides a unified interface for storing and retrieving build
//! artifacts across multiple backends (local filesystem, S3, Mœ, etc.)
//!
//! ## Features
//! - Multi-backend support: local filesystem, S3, Mœ
//! - Cache hit/miss tracking
//! - Automatic cache cleanup
//! - Storage statistics
//! - Content-addressable storage (CAS)
//!
//! ## Backends
//! - **Local**: Simple filesystem storage
//! - **S3**: AWS S3-compatible storage  
//! - **Mœ**: Self-sovereign storage for opendesk
//!
//! ## Messages Handled
//! - StoreObject: Store data in storage
//! - LoadObject: Retrieve data from storage
//! - CheckCache: Check if object exists in cache
//! - UploadToCache: Upload data to cache

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use thiserror::Error;
use tokio::sync::{mpsc, RwLock};

use agentflow_core::{
    Agent, AgentContext, AgentDefinition, AgentStatus, AgentType, AgentMessage, Result,
};
use agentflow_core::message::CacheBackendStats;

/// Storage backend configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackendConfig {
    /// Local filesystem storage
    Local {
        path: String,
        max_size: Option<u64>,
        cleanup_days: Option<u64>,
    },
    
    /// S3-compatible storage
    S3 {
        endpoint: String,
        bucket: String,
        access_key: Option<String>,
        secret_key: Option<String>,
        region: Option<String>,
        use_https: bool,
        prefix: Option<String>,
    },
    
    /// Mœ self-sovereign storage
    Moe {
        endpoint: String,
        identity: String,
        namespace: String,
        generation: Option<u64>,
    },
}

/// Storage Manager configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default)]
    pub backends: Vec<BackendConfig>,
    #[serde(default = "default_primary")]
    pub primary_backend: String,
    #[serde(default = "default_true")]
    pub cache_enabled: bool,
    #[serde(default)]
    pub cache_ttl: Option<u64>,
    #[serde(default)]
    pub max_cache_size: Option<u64>,
    #[serde(default = "default_true")]
    pub auto_cleanup: bool,
    #[serde(default = "default_cleanup_interval")]
    pub cleanup_interval: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backends: vec![],
            primary_backend: default_primary(),
            cache_enabled: true,
            cache_ttl: None,
            max_cache_size: None,
            auto_cleanup: true,
            cleanup_interval: default_cleanup_interval(),
        }
    }
}

fn default_primary() -> String { "local".to_string() }
fn default_true() -> bool { true }
fn default_cleanup_interval() -> u64 { 3600 }

impl StorageConfig {
    pub fn get_primary(&self) -> Option<&BackendConfig> {
        for backend in &self.backends {
            match backend {
                BackendConfig::Local { .. } => if self.primary_backend == "local" { return Some(backend) }
                BackendConfig::S3 { .. } => if self.primary_backend == "s3" { return Some(backend) }
                BackendConfig::Moe { .. } => if self.primary_backend == "moe" { return Some(backend) }
            }
        }
        self.backends.first()
    }
}

/// Object metadata for cache tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub hash: String,
    pub size: u64,
    pub backend: String,
    pub timestamp: DateTime<Utc>,
    pub ttl: Option<u64>,
    pub accesses: u64,
}

/// Cache statistics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheStatistics {
    pub total_objects: u64,
    pub total_size: u64,
    pub hit_count: u64,
    pub miss_count: u64,
    pub hit_rate: f32,
    pub evictions: u64,
    pub backends: HashMap<String, CacheBackendStats>,
}

/// Storage Manager Agent
pub struct StorageManagerAgent {
    definition: AgentDefinition,
    sender: mpsc::Sender<AgentMessage>,
    task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    config: StorageConfig,
    backends: HashMap<String, Arc<dyn StorageBackend + Send + Sync>>,
    cache: Arc<RwLock<HashMap<String, CacheEntry>>>,
    stats: Arc<RwLock<CacheStatistics>>,
}

/// Error type for Storage Manager
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Storage backend error: {0}")]
    BackendError(String),
    #[error("Object not found: {0}")]
    NotFound(String),
    #[error("Storage full: {0}")]
    StorageFull(String),
    #[error("Channel send error")]
    ChannelSendError,
}

impl From<mpsc::error::SendError<AgentMessage>> for StorageError {
    fn from(_: mpsc::error::SendError<AgentMessage>) -> Self {
        StorageError::ChannelSendError
    }
}

/// Storage backend trait
#[async_trait]
pub trait StorageBackend: Send + Sync {
    fn name(&self) -> &str;
    async fn store(&self, hash: &str, data: &[u8], metadata: &HashMap<String, String>) -> Result<StorageResult>;
    async fn load(&self, hash: &str) -> Result<Option<Vec<u8>>>;
    async fn exists(&self, hash: &str) -> Result<bool>;
    async fn delete(&self, hash: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<String>>;
    async fn get_metadata(&self, hash: &str) -> Result<Option<HashMap<String, String>>>;
    async fn stats(&self) -> Result<CacheBackendStats>;
}

/// Result of a storage operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageResult {
    pub hash: String,
    pub size: u64,
    pub location: String,
    pub timestamp: DateTime<Utc>,
}

impl StorageManagerAgent {
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
        config: StorageConfig,
    ) -> Self {
        let mut capabilities = HashSet::new();
        capabilities.insert("storage".to_string());
        capabilities.insert("cache".to_string());
        capabilities.insert("upload".to_string());
        capabilities.insert("download".to_string());
        capabilities.insert("moe-storage".to_string());
        capabilities.insert("s3-storage".to_string());
        capabilities.insert("local-storage".to_string());
        
        // Initialize backends
        let mut backends = HashMap::new();
        for (idx, backend_config) in config.backends.iter().enumerate() {
            let backend_name = match backend_config {
                BackendConfig::Local { .. } => "local",
                BackendConfig::S3 { .. } => "s3",
                BackendConfig::Moe { .. } => "moe",
            };
            let key = if config.backends.len() == 1 {
                backend_name.to_string()
            } else {
                format!("{}-{}", backend_name, idx)
            };
            
            let backend: Arc<dyn StorageBackend + Send + Sync> = match backend_config {
                BackendConfig::Local { path, .. } => Arc::new(LocalBackend::new(path.clone())),
                BackendConfig::S3 { endpoint, bucket, access_key, secret_key, region, use_https, prefix } => {
                    Arc::new(S3Backend::new(
                        endpoint.clone(), bucket.clone(),
                        access_key.clone(), secret_key.clone(),
                        region.clone(), *use_https, prefix.clone(),
                    ))
                }
                BackendConfig::Moe { endpoint, identity, namespace, generation } => {
                    Arc::new(MoeBackend::new(
                        endpoint.clone(), identity.clone(), namespace.clone(), *generation,
                    ))
                }
            };
            backends.insert(key, backend);
        }
        
        let definition = AgentDefinition {
            id: format!("storage-manager-{}", Utc::now().timestamp()),
            name: "Storage Manager Agent".to_string(),
            agent_type: AgentType::StorageManager,
            status: AgentStatus::Ready,
            capabilities,
            config: serde_json::to_value(&config).unwrap_or(serde_json::Value::Null),
            ..Default::default()
        };
        
        Self {
            definition,
            sender,
            task_store,
            config,
            backends,
            cache: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(RwLock::new(CacheStatistics::default())),
        }
    }
    
    pub fn from_definition(
        definition: &AgentDefinition,
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    ) -> Result<Self> {
        let config: StorageConfig = if definition.config == serde_json::Value::Null {
            StorageConfig::default()
        } else {
            serde_json::from_value(definition.config.clone())
                .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?
        };
        
        Ok(Self::new(sender, task_store, config))
    }
    
    fn get_primary_backend(&self) -> Option<Arc<dyn StorageBackend + Send + Sync>> {
        if let Some(backend) = self.backends.get(&self.config.primary_backend) {
            return Some(backend.clone());
        }
        self.backends.values().next().cloned()
    }
    
    fn get_backend(&self, name: &str) -> Option<Arc<dyn StorageBackend + Send + Sync>> {
        self.backends.get(name).cloned()
    }
    
    fn compute_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    }
    
    async fn store_object(
        &self,
        hash: String,
        data: Vec<u8>,
        content_type: String,
        metadata: HashMap<String, String>,
        task_id: String,
    ) -> Result<StorageResult> {
        let backend = self.get_primary_backend()
            .ok_or(agentflow_core::error::AgentFlowError::Generic(
                "No storage backend configured".to_string()
            ))?;
        
        let mut meta = metadata.clone();
        meta.insert("content_type".to_string(), content_type);
        meta.insert("size".to_string(), data.len().to_string());
        
        let result = backend.store(&hash, &data, &meta).await?;
        
        // Update cache
        {
            let mut cache = self.cache.write().await;
            cache.insert(hash.clone(), CacheEntry {
                hash: hash.clone(),
                size: result.size,
                backend: backend.name().to_string(),
                timestamp: result.timestamp,
                ttl: self.config.cache_ttl,
                accesses: 0,
            });
        }
        
        // Update stats
        {
            let mut stats = self.stats.write().await;
            stats.total_objects += 1;
            stats.total_size += result.size;
        }
        
        Ok(result)
    }
    
    async fn load_object(&self, hash: String, task_id: String) -> Result<Option<Vec<u8>>> {
        // First check cache for backend info
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.get(&hash) {
                if let Some(backend) = self.get_backend(&entry.backend) {
                    if let Some(data) = backend.load(&hash).await? {
                        // Update access count
                        drop(cache);
                        let mut cache = self.cache.write().await;
                        if let Some(entry) = cache.get_mut(&hash) {
                            entry.accesses += 1;
                        }
                        
                        let mut stats = self.stats.write().await;
                        stats.hit_count += 1;
                        
                        return Ok(Some(data));
                    }
                }
            }
        }
        
        // Try primary backend
        if let Some(backend) = self.get_primary_backend() {
            if let Some(data) = backend.load(&hash).await? {
                // Update cache on miss but found in backend
                {
                    let mut stats = self.stats.write().await;
                    stats.miss_count += 1;
                }
                return Ok(Some(data));
            }
        }
        
        // Try all other backends
        for (_, backend) in &self.backends {
            if let Some(data) = backend.load(&hash).await? {
                {
                    let mut stats = self.stats.write().await;
                    stats.miss_count += 1;
                }
                return Ok(Some(data));
            }
        }
        
        Ok(None)
    }
    
    async fn check_cache(&self, hash: String, task_id: String) -> Result<bool> {
        // Check cache
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.get(&hash) {
                // Check TTL
                if let Some(ttl) = entry.ttl {
                    let age = Utc::now().signed_duration_since(entry.timestamp).num_seconds() as u64;
                    if age > ttl {
                        return Ok(false); // Expired
                    }
                }
                return Ok(true);
            }
        }
        
        // Check backends
        if let Some(backend) = self.get_primary_backend() {
            return backend.exists(&hash).await;
        }
        
        Ok(false)
    }
    
    async fn get_stats(&self) -> CacheStatistics {
        let stats = self.stats.read().await;
        let mut result = stats.clone();
        
        // Update hit rate
        let total = result.hit_count + result.miss_count;
        if total > 0 {
            result.hit_rate = (result.hit_count as f32 / total as f32) * 100.0;
        }
        
        // Get backend stats
        for (name, backend) in &self.backends {
            if let Ok(backend_stats) = backend.stats().await {
                result.backends.insert(name.clone(), backend_stats);
            }
        }
        
        result
    }
    
    fn update_task_status(&self, task_id: String, status: agentflow_core::task::TaskStatus) {
        if task_id.is_empty() { return; }
        let task_store = self.task_store.clone();
        let update = agentflow_core::agent::TaskUpdate {
            status: Some(status),
            ..Default::default()
        };
        tokio::spawn(async move {
            let _ = task_store.update_task(&task_id, update).await;
        });
    }
}

// ========== Backend Implementations ==========

pub struct LocalBackend {
    path: PathBuf,
}

impl LocalBackend {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path).ok();
        Self { path }
    }
    
    fn object_path(&self, hash: &str) -> PathBuf {
        self.path.join(hash)
    }
    
    fn metadata_path(&self, hash: &str) -> PathBuf {
        self.path.join(format!("{}.meta", hash))
    }
}

#[async_trait]
impl StorageBackend for LocalBackend {
    fn name(&self) -> &str { "local" }
    
    async fn store(&self, hash: &str, data: &[u8], metadata: &HashMap<String, String>) -> Result<StorageResult> {
        let path = self.object_path(hash);
        let meta_path = self.metadata_path(hash);
        
        tokio::fs::write(&path, data).await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        
        let meta_json = serde_json::to_string(metadata)
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        tokio::fs::write(&meta_path, meta_json).await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        
        Ok(StorageResult {
            hash: hash.to_string(),
            size: data.len() as u64,
            location: path.to_string_lossy().into_owned(),
            timestamp: Utc::now(),
        })
    }
    
    async fn load(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let path = self.object_path(hash);
        if !path.exists() { return Ok(None); }
        let data = tokio::fs::read(&path).await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        Ok(Some(data))
    }
    
    async fn exists(&self, hash: &str) -> Result<bool> {
        let path = self.object_path(hash);
        Ok(path.exists())
    }
    
    async fn delete(&self, hash: &str) -> Result<()> {
        let path = self.object_path(hash);
        let meta_path = self.metadata_path(hash);
        tokio::fs::remove_file(&path).await.ok();
        tokio::fs::remove_file(&meta_path).await.ok();
        Ok(())
    }
    
    async fn list(&self) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        if self.path.exists() {
            for entry in std::fs::read_dir(&self.path)
                .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?
            {
                let entry = entry.map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
                if let Some(name) = entry.file_name().to_str() {
                    if !name.ends_with(".meta") {
                        paths.push(name.to_string());
                    }
                }
            }
        }
        Ok(paths)
    }
    
    async fn get_metadata(&self, hash: &str) -> Result<Option<HashMap<String, String>>> {
        let meta_path = self.metadata_path(hash);
        if !meta_path.exists() { return Ok(None); }
        let meta_json = tokio::fs::read_to_string(&meta_path).await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        let metadata: HashMap<String, String> = serde_json::from_str(&meta_json)
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        Ok(Some(metadata))
    }
    
    async fn stats(&self) -> Result<CacheBackendStats> {
        let paths = self.list().await?;
        let mut total_size = 0u64;
        for hash in &paths {
            if let Some(data) = self.load(hash).await? {
                total_size += data.len() as u64;
            }
        }
        Ok(CacheBackendStats {
            object_count: paths.len() as u64,
            total_size,
            hit_count: 0,
            miss_count: 0,
        })
    }
}

pub struct S3Backend {
    endpoint: String,
    bucket: String,
    access_key: Option<String>,
    secret_key: Option<String>,
    region: Option<String>,
    use_https: bool,
    prefix: Option<String>,
    client: reqwest::Client,
}

impl S3Backend {
    pub fn new(
        endpoint: String, bucket: String,
        access_key: Option<String>, secret_key: Option<String>,
        region: Option<String>, use_https: bool, prefix: Option<String>,
    ) -> Self {
        Self {
            endpoint, bucket, access_key, secret_key, region,
            use_https, prefix,
            client: reqwest::Client::new(),
        }
    }
    
    fn object_key(&self, hash: &str) -> String {
        match &self.prefix {
            Some(p) => format!("{}/{}", p, hash),
            None => hash.to_string(),
        }
    }
    
    fn url(&self, key: &str) -> String {
        let scheme = if self.use_https { "https" } else { "http" };
        format!("{}://{}/{}/{}", scheme, self.endpoint, self.bucket, key)
    }
    
    async fn get_headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        if let (Some(ak), Some(sk)) = (&self.access_key, &self.secret_key) {
            if sk.contains('.') {
                headers.push(("Authorization".to_string(), format!("Bearer {}", sk)));
            }
        }
        headers
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    fn name(&self) -> &str { "s3" }
    
    async fn store(&self, hash: &str, data: &[u8], _metadata: &HashMap<String, String>) -> Result<StorageResult> {
        let key = self.object_key(hash);
        let url = self.url(&key);
        let headers = self.get_headers().await;
        
        let mut req = self.client.put(&url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        
        let response = req.body(data.to_vec()).send().await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        
        if !response.status().is_success() {
            return Err(agentflow_core::error::AgentFlowError::Generic(format!(
                "S3 upload failed: {}", response.status()
            )));
        }
        
        Ok(StorageResult {
            hash: hash.to_string(),
            size: data.len() as u64,
            location: format!("s3://{}/{}", self.bucket, key),
            timestamp: Utc::now(),
        })
    }
    
    async fn load(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let key = self.object_key(hash);
        let url = self.url(&key);
        let headers = self.get_headers().await;
        
        let mut req = self.client.get(&url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        
        let response = req.send().await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(agentflow_core::error::AgentFlowError::Generic(format!(
                "S3 download failed: {}", response.status()
            )));
        }
        
        let data = response.bytes().await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        Ok(Some(data.to_vec()))
    }
    
    async fn exists(&self, hash: &str) -> Result<bool> {
        Ok(self.load(hash).await?.is_some())
    }
    
    async fn delete(&self, hash: &str) -> Result<()> {
        let key = self.object_key(hash);
        let url = self.url(&key);
        let headers = self.get_headers().await;
        
        let mut req = self.client.delete(&url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        
        let response = req.send().await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        
        if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(agentflow_core::error::AgentFlowError::Generic(format!(
                "S3 delete failed: {}", response.status()
            )));
        }
        Ok(())
    }
    
    async fn list(&self) -> Result<Vec<String>> {
        Ok(Vec::new()) // Simplified
    }
    
    async fn get_metadata(&self, _hash: &str) -> Result<Option<HashMap<String, String>>> {
        Ok(None) // Simplified
    }
    
    async fn stats(&self) -> Result<CacheBackendStats> {
        Ok(CacheBackendStats::default())
    }
}

pub struct MoeBackend {
    endpoint: String,
    identity: String,
    namespace: String,
    generation: Option<u64>,
    client: reqwest::Client,
}

impl MoeBackend {
    pub fn new(endpoint: String, identity: String, namespace: String, generation: Option<u64>) -> Self {
        Self {
            endpoint, identity, namespace, generation,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl StorageBackend for MoeBackend {
    fn name(&self) -> &str { "moe" }
    
    async fn store(&self, hash: &str, data: &[u8], metadata: &HashMap<String, String>) -> Result<StorageResult> {
        let url = format!("{}/api/v1/objects", self.endpoint);
        let mut headers = Vec::new();
        headers.push(("X-Moe-Identity".to_string(), self.identity.clone()));
        if let Some(gen) = self.generation {
            headers.push(("X-Moe-Generation".to_string(), gen.to_string()));
        }
        
        let body = serde_json::json!({
            "namespace": self.namespace,
            "hash": hash,
            "data": data,
            "metadata": metadata,
        });
        
        let response = self.client.post(&url)
            .headers(headers.into_iter().map(|(k, v)| {
                (reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                 reqwest::header::HeaderValue::from_str(&v).unwrap())
            }).collect())
            .json(&body)
            .send().await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        
        if !response.status().is_success() {
            return Err(agentflow_core::error::AgentFlowError::Generic(format!(
                "Mœ upload failed: {}", response.status()
            )));
        }
        
        Ok(StorageResult {
            hash: hash.to_string(),
            size: data.len() as u64,
            location: format!("moe://{}/{}/{}", self.identity, self.namespace, hash),
            timestamp: Utc::now(),
        })
    }
    
    async fn load(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/api/v1/objects/{}", self.endpoint, hash);
        let mut headers = Vec::new();
        headers.push(("X-Moe-Identity".to_string(), self.identity.clone()));
        
        let response = self.client.get(&url)
            .headers(headers.into_iter().map(|(k, v)| {
                (reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                 reqwest::header::HeaderValue::from_str(&v).unwrap())
            }).collect())
            .send().await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(agentflow_core::error::AgentFlowError::Generic(format!(
                "Mœ load failed: {}", response.status()
            )));
        }
        
        let bytes = response.bytes().await
            .map_err(|e| agentflow_core::error::AgentFlowError::Generic(e.to_string()))?;
        Ok(Some(bytes.to_vec()))
    }
    
    async fn exists(&self, hash: &str) -> Result<bool> {
        Ok(self.load(hash).await?.is_some())
    }
    
    async fn delete(&self, _hash: &str) -> Result<()> { Ok(()) }
    async fn list(&self) -> Result<Vec<String>> { Ok(Vec::new()) }
    async fn get_metadata(&self, _hash: &str) -> Result<Option<HashMap<String, String>>> { Ok(None) }
    async fn stats(&self) -> Result<CacheBackendStats> { Ok(CacheBackendStats::default()) }
}

// ========== Agent Trait Implementation ==========

#[async_trait]
impl Agent for StorageManagerAgent {
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
            AgentMessage::StoreObject { data, content_type, metadata, task_id } => {
                let hash = Self::compute_hash(&data);
                match self.store_object(hash.clone(), data, content_type, metadata, task_id.clone()).await {
                    Ok(result) => {
                        let response = AgentMessage::ObjectStored {
                            hash: result.hash,
                            size: result.size,
                            storage_location: result.location,
                            task_id,
                        };
                        self.sender.send(response).await?;
                    }
                    Err(e) => {
                        tracing::error!("Store object failed: {}", e);
                    }
                }
            }
            
            AgentMessage::LoadObject { hash, task_id } => {
                match self.load_object(hash.clone(), task_id.clone()).await {
                    Ok(Some(data)) => {
                        let response = AgentMessage::ObjectLoaded {
                            hash,
                            data,
                            task_id,
                        };
                        self.sender.send(response).await?;
                    }
                    Ok(None) => {
                        tracing::warn!("Object not found: {}", hash);
                    }
                    Err(e) => {
                        tracing::error!("Load object failed: {}", e);
                    }
                }
            }
            
            AgentMessage::CheckCache { hash, task_id } => {
                match self.check_cache(hash.clone(), task_id.clone()).await {
                    Ok(exists) => {
                        let response = AgentMessage::CacheCheckResult {
                            hash,
                            exists,
                            location: if exists { Some("cache".to_string()) } else { None },
                            size: None,
                            task_id,
                        };
                        self.sender.send(response).await?;
                    }
                    Err(e) => {
                        tracing::error!("Check cache failed: {}", e);
                    }
                }
            }
            
            AgentMessage::UploadToCache { hash, data, content_type, metadata, task_id } => {
                match self.store_object(hash.clone(), data, content_type, metadata, task_id.clone()).await {
                    Ok(result) => {
                        self.sender.send(AgentMessage::CacheUploaded {
                            hash: result.hash,
                            size: result.size,
                            storage_backend: result.location,
                            task_id,
                        }).await?;
                    }
                    Err(e) => {
                        tracing::error!("Upload to cache failed: {}", e);
                    }
                }
            }
            
            _ => {
                tracing::debug!("StorageManager ignoring message");
            }
        }
        
        Ok(())
    }
    
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> {
        tracing::info!("StorageManagerAgent started");
        self.definition.status = AgentStatus::Ready;
        self.sender.send(AgentMessage::AgentReady {
            agent_id: self.definition.id.clone(),
        }).await?;
        Ok(())
    }
    
    async fn on_shutdown(&mut self) -> Result<()> {
        tracing::info!("StorageManagerAgent shutting down");
        self.definition.status = AgentStatus::Stopping;
        Ok(())
    }
    
    fn status(&self) -> AgentStatus {
        self.definition.status.clone()
    }
}

// ========== Tests ==========

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_hash_computation() {
        let hash1 = StorageManagerAgent::compute_hash(b"test");
        let hash2 = StorageManagerAgent::compute_hash(b"test");
        let hash3 = StorageManagerAgent::compute_hash(b"different");
        
        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        assert_eq!(hash1.len(), 64);
    }
    
    #[test]
    fn test_config_default() {
        let config = StorageConfig::default();
        assert!(config.cache_enabled);
        assert_eq!(config.cleanup_interval, 3600);
    }
}
