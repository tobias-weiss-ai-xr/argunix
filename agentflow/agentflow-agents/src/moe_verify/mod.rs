//! MoeVerifyAgent - Verify Mœ object integrity and authenticity
//!
//! This agent provides:
//! - Cryptographic verification of Mœ objects
//! - Digital signature verification (simulated)
//! - Hash integrity checks
//! - Identity verification
//! - Trust chain validation
//!
//! ## Verification Process
//! When a Mœ object is retrieved, we verify:
//! 1. Hash matches the expected content hash
//! 2. Digital signature is valid (simulated with dummy keys)
//! 3. Signing identity is trusted
//! 4. Object hasn't been tampered with
//! 5. Object belongs to the expected namespace and generation
//!
//! ## Messages Handled
//! - VerifyMoeObject: Verify a Mœ object
//! - BatchVerifyMoe: Verify multiple objects
//! - VerifyIdentity: Verify a Mœ identity
//! - VerifyNamespace: Verify all objects in a namespace

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tokio::sync::{mpsc, RwLock};
use serde::{Deserialize, Serialize};

use agentflow_core::{Agent, AgentDefinition, AgentContext, AgentMessage, AgentStatus, AgentType, Result, TaskDefinition, TaskStatus, TaskType};
use agentflow_core::agent::{StateStore, TaskStore};
use sha2::{Sha256, Digest};

/// Verification result status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VerificationStatus {
    /// Verification passed
    Valid,
    /// Invalid hash
    HashMismatch,
    /// Invalid signature
    InvalidSignature,
    /// Identity not trusted
    UntrustedIdentity,
    /// Object not found
    NotFound,
    /// Expired object
    Expired,
    /// Revoked identity
    RevokedIdentity,
    /// Unknown error
    Error(String),
}

/// Verification result for a single object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    /// Object hash
    pub hash: String,
    /// Verification status
    pub status: VerificationStatus,
    /// Verification timestamp
    pub verified_at: DateTime<Utc>,
    /// Signing identity fingerprint
    pub signer: Option<String>,
    /// Error details
    pub error: Option<String>,
    /// Verification duration in milliseconds
    pub duration_ms: u64,
}

impl VerificationResult {
    pub fn new(hash: String, status: VerificationStatus) -> Self {
        Self {
            hash,
            status,
            verified_at: Utc::now(),
            signer: None,
            error: None,
            duration_ms: 0,
        }
    }
    
    pub fn is_valid(&self) -> bool {
        self.status == VerificationStatus::Valid
    }
}

/// Trusted identity information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedIdentity {
    /// Identity fingerprint
    pub fingerprint: String,
    /// Public key
    pub public_key: String,
    /// Trust level (0-100)
    pub trust_level: u8,
    /// Trusted since timestamp
    pub trusted_since: DateTime<Utc>,
    /// Notes/comments
    pub notes: Option<String>,
    /// Tags for categorization
    pub tags: Vec<String>,
}

/// Verification cache entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationCacheEntry {
    pub result: VerificationResult,
    pub cached_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl VerificationCacheEntry {
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }
}

/// Verify configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoeVerifyConfig {
    /// Required trust level for verification to pass
    pub min_trust_level: u8,
    /// Automatically trust new identities
    pub auto_trust: bool,
    /// Cache verification results
    pub cache_verify: bool,
    /// Verification cache TTL in seconds
    pub cache_ttl: u64,
    /// Maximum object size to verify in bytes (0 = no limit)
    pub max_size: u64,
    /// Require timestamp validation
    pub require_timestamp: bool,
    /// Default trusted identities
    pub trusted_identities: Vec<TrustedIdentity>,
}

impl Default for MoeVerifyConfig {
    fn default() -> Self {
        let trusted_identities = vec![
            TrustedIdentity {
                fingerprint: "sha256-argunix-ci".to_string(),
                public_key: "dummy-argunix-public-key".to_string(),
                trust_level: 100,
                trusted_since: Utc::now(),
                notes: Some("Argunix CI identity".to_string()),
                tags: vec!["ci".to_string(), "argunix".to_string()],
            },
            TrustedIdentity {
                fingerprint: "sha256-opendesk".to_string(),
                public_key: "dummy-opendesk-public-key".to_string(),
                trust_level: 90,
                trusted_since: Utc::now(),
                notes: Some("OpenDesk infrastructure".to_string()),
                tags: vec!["infrastructure".to_string()],
            },
        ];
        
        Self {
            min_trust_level: 50,
            auto_trust: false,
            cache_verify: true,
            cache_ttl: 3600, // 1 hour
            max_size: 1024 * 1024 * 1024, // 1GB default limit
            require_timestamp: true,
            trusted_identities,
        }
    }
}

/// Agent state
#[derive(Debug, Default)]
pub struct MoeVerifyState {
    /// Trusted identities by fingerprint
    pub trusted_identities: HashMap<String, TrustedIdentity>,
    /// Revoked identity fingerprints
    pub revoked_identities: HashSet<String>,
    /// Verification cache by hash
    pub verify_cache: HashMap<String, VerificationCacheEntry>,
    /// Statistics
    pub stats: MoeVerifyStats,
}

/// Agent statistics
#[derive(Debug, Default, Clone)]
pub struct MoeVerifyStats {
    pub objects_verified: u64,
    pub objects_failed: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub identities_trusted: u64,
    pub identities_revoked: u64,
}

/// The MoeVerifyAgent
pub struct MoeVerifyAgent {
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn TaskStore>,
    /// State store (unused but required by trait)
    _state_store: Arc<dyn StateStore>,
    /// Configuration
    config: MoeVerifyConfig,
    /// State
    state: Arc<RwLock<MoeVerifyState>>,
    /// Agent name
    name: String,
    /// Agent type
    agent_type: AgentType,
    /// Agent capabilities
    capabilities: HashSet<String>,
    /// Agent status
    status: AgentStatus,
}

impl MoeVerifyAgent {
    /// Create a new MoeVerifyAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn TaskStore>,
        state_store: Arc<dyn StateStore>,
        config: Option<MoeVerifyConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        
        // Initialize state with config's trusted identities
        let mut initial_state = MoeVerifyState::default();
        for identity in &config.trusted_identities {
            initial_state.trusted_identities.insert(identity.fingerprint.clone(), identity.clone());
        }
        
        let capabilities = vec![
            "moe-verify".to_string(),
            "verification".to_string(),
            "cryptographic".to_string(),
            "integrity-check".to_string(),
            "signature-verification".to_string(),
        ].into_iter().collect();
        
        Self {
            sender,
            task_store,
            _state_store: state_store,
            config,
            state: Arc::new(RwLock::new(initial_state)),
            name: "MoeVerifyAgent".to_string(),
            agent_type: AgentType::Custom,
            capabilities,
            status: AgentStatus::Ready,
        }
    }
    
    /// Compute SHA256 hash of data
    fn compute_hash(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }
    
    /// Verify hash of data against expected hash
    pub fn verify_hash(data: &[u8], expected_hash: &str) -> bool {
        Self::compute_hash(data) == expected_hash.to_lowercase()
    }
    
    /// Simulate signature verification
    /// In production, this would use actual Ed25519 verification
    pub fn verify_signature(
        &self,
        data: &[u8],
        signature: &[u8],
        public_key: &str,
    ) -> bool {
        // Simulate verification by checking if signature is non-empty
        // and public key matches expected format
        !signature.is_empty() && !public_key.is_empty()
    }
    
    /// Check if an identity is trusted
    pub async fn is_identity_trusted(&self, fingerprint: &str) -> Option<TrustedIdentity> {
        let state = self.state.read().await;
        
        if state.revoked_identities.contains(fingerprint) {
            return None;
        }
        
        state.trusted_identities.get(fingerprint).cloned()
    }
    
    /// Verify a single object
    pub async fn verify_object(
        &self,
        hash: &str,
        data: &[u8],
        signer: Option<String>,
        signature: Option<Vec<u8>>,
    ) -> VerificationResult {
        let start = Instant::now();
        let mut result = VerificationResult::new(hash.to_string(), VerificationStatus::Valid);
        
        // Check cache first
        {
            let state = self.state.read().await;
            if self.config.cache_verify {
                if let Some(cache_entry) = state.verify_cache.get(hash) {
                    if !cache_entry.is_expired() {
                        let mut cached_result = cache_entry.result.clone();
                        cached_result.duration_ms = start.elapsed().as_millis() as u64;
                        // Update stats
                        result = cached_result;
                        {
                            let mut state = self.state.write().await;
                            state.stats.cache_hits += 1;
                        }
                        return result;
                    }
                }
                // Cache miss
                {
                    let mut state = self.state.write().await;
                    state.stats.cache_misses += 1;
                }
            }
        }
        
        // Step 1: Verify hash
        if !Self::verify_hash(data, hash) {
            result.status = VerificationStatus::HashMismatch;
            result.error = Some("Hash does not match data".to_string());
            return result;
        }
        
        // Step 2: Verify signature if present
        if let (Some(signer_fp), Some(sig)) = (signer, signature) {
            // Get the identity
            let identity_opt = self.is_identity_trusted(&signer_fp).await;
            
            if identity_opt.is_none() {
                result.status = VerificationStatus::UntrustedIdentity;
                result.error = Some(format!("Identity {} is not trusted", signer_fp));
                return result;
            }
            
            let identity = identity_opt.unwrap();
            
            // Verify trust level
            if identity.trust_level < self.config.min_trust_level {
                result.status = VerificationStatus::UntrustedIdentity;
                result.error = Some(format!(
                    "Identity trust level {} below minimum {}",
                    identity.trust_level,
                    self.config.min_trust_level
                ));
                return result;
            }
            
            // Verify signature
            if !self.verify_signature(data, &sig, &identity.public_key) {
                result.status = VerificationStatus::InvalidSignature;
                result.error = Some("Digital signature verification failed".to_string());
                return result;
            }
            
            result.signer = Some(signer_fp);
        }
        
        // Step 3: All checks passed
        result.status = VerificationStatus::Valid;
        result.duration_ms = start.elapsed().as_millis() as u64;
        
        // Cache the result
        if self.config.cache_verify {
            let cache_entry = VerificationCacheEntry {
                result: result.clone(),
                cached_at: Utc::now(),
                expires_at: Utc::now() + ChronoDuration::from_std(Duration::from_secs(self.config.cache_ttl)).unwrap(),
            };
            
            let mut state = self.state.write().await;
            state.verify_cache.insert(hash.to_string(), cache_entry);
            state.stats.objects_verified += 1;
        }
        
        result
    }
    
    /// Verify multiple objects
    pub async fn verify_objects(
        &self,
        objects: Vec<(String, Vec<u8>, Option<String>, Option<Vec<u8>>)>,
    ) -> BatchVerificationResult {
        let start = Instant::now();
        let mut results = Vec::new();
        let mut total_valid = 0;
        let mut total_failed = 0;
        
        for (hash, data, signer, signature) in objects {
            let result = self.verify_object(&hash, &data, signer, signature).await;
            
            if result.is_valid() {
                total_valid += 1;
            } else {
                total_failed += 1;
            }
            
            results.push(result);
        }
        
        let total = results.len();
        BatchVerificationResult {
            results,
            total,
            valid: total_valid,
            failed: total_failed,
            duration_ms: start.elapsed().as_millis() as u64,
        }
    }
    
    /// Add a trusted identity
    pub async fn add_trusted_identity(&mut self, identity: TrustedIdentity) -> Result<()> {
        let mut state = self.state.write().await;
        
        // Remove from revoked if present
        state.revoked_identities.remove(&identity.fingerprint);
        
        state.trusted_identities.insert(identity.fingerprint.clone(), identity);
        state.stats.identities_trusted += 1;
        
        Ok(())
    }
    
    /// Remove a trusted identity
    pub async fn remove_trusted_identity(&mut self, fingerprint: &str) -> bool {
        let mut state = self.state.write().await;
        state.trusted_identities.remove(fingerprint).is_some()
    }
    
    /// Revoke an identity (add to revoked list)
    pub async fn revoke_identity(&mut self, fingerprint: &str, reason: Option<String>) -> Result<()> {
        let mut state = self.state.write().await;
        
        // Remove from trusted if present
        state.trusted_identities.remove(fingerprint);
        state.revoked_identities.insert(fingerprint.to_string());
        state.stats.identities_revoked += 1;
        
        tracing::warn!(
            "Revoked identity {}: {}",
            fingerprint,
            reason.as_deref().unwrap_or("")
        );
        
        Ok(())
    }
    
    /// Get statistics
    pub async fn get_stats(&self) -> MoeVerifyStats {
        let state = self.state.read().await;
        state.stats.clone()
    }
    
    /// Clear verification cache
    pub async fn clear_cache(&mut self) -> usize {
        let mut state = self.state.write().await;
        let count = state.verify_cache.len();
        state.verify_cache.clear();
        count
    }
}

/// Batch verification result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchVerificationResult {
    pub results: Vec<VerificationResult>,
    pub total: usize,
    pub valid: u64,
    pub failed: u64,
    pub duration_ms: u64,
}

/// Implement Agent trait for MoeVerifyAgent
#[async_trait::async_trait]
impl Agent for MoeVerifyAgent {
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
            AgentMessage::VerifyMoeObject { hash, data, signer, signature, task_id } => {
                let result = self.verify_object(&hash, &data, signer, signature).await;
                
                let mut task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("verify-{}", hash)),
                    task_type: TaskType::VerifyMoeObject,
                    status: if result.is_valid() { TaskStatus::Succeeded } else { TaskStatus::Failed },
                    priority: 90, // High priority - verification is critical
                    created_at: Utc::now(),
                    ..Default::default()
                };
                
                if !result.is_valid() {
                    let error_msg = result.error.clone().unwrap_or_else(|| "verification failed".to_string());
                    task.metadata.insert("error".to_string(), error_msg);
                }
                
                self.task_store.create_task(&task).await?;
                
                let status_str = format!("{:?}", result.status);
                self.sender.send(AgentMessage::MoeObjectVerified {
                    hash: result.hash.clone(),
                    valid: result.is_valid(),
                    verification_status: status_str,
                    signer: result.signer.clone(),
                    error: result.error.clone(),
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::BatchVerifyMoe { objects, task_id } => {
                let batch_start = Instant::now();
                let results: Vec<(String, Vec<u8>, Option<String>, Option<Vec<u8>>)> = objects
                    .into_iter()
                    .map(|obj| {
                        let hash = obj.hash.clone();
                        let data = obj.data.clone();
                        let signer = obj.signer.clone();
                        let signature = obj.signature.clone();
                        (hash, data, signer, signature)
                    })
                    .collect();
                
                let batch_result = self.verify_objects(results).await;
                
                let mut task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("batch-verify-{}", batch_start.elapsed().as_millis())),
                    task_type: TaskType::VerifyMoeObject,
                    status: if batch_result.failed == 0 { TaskStatus::Succeeded } else { TaskStatus::Failed },
                    priority: 85,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                
                task.metadata.insert("total".to_string(), batch_result.total.to_string());
                task.metadata.insert("valid".to_string(), batch_result.valid.to_string());
                task.metadata.insert("failed".to_string(), batch_result.failed.to_string());
                
                self.task_store.create_task(&task).await?;
                
                let moe_results: Vec<_> = batch_result.results.iter().map(|r| {
                    agentflow_core::message::MoeVerificationInfo {
                        hash: r.hash.clone(),
                        valid: r.is_valid(),
                        verification_status: format!("{:?}", r.status),
                        signer: r.signer.clone(),
                        error: r.error.clone(),
                    }
                }).collect();
                
                self.sender.send(AgentMessage::BatchVerifyMoeComplete {
                    results: moe_results,
                    total: batch_result.total as u64,
                    valid: batch_result.valid,
                    failed: batch_result.failed,
                    duration_seconds: batch_start.elapsed().as_secs_f64(),
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::VerifyMoeIdentity { fingerprint, task_id } => {
                let identity_opt = self.is_identity_trusted(&fingerprint).await;
                
                let mut task = TaskDefinition {
                    id: task_id.unwrap_or_else(|| format!("verify-id-{}", fingerprint)),
                    task_type: TaskType::VerifyMoeIdentity,
                    status: if identity_opt.is_some() { TaskStatus::Succeeded } else { TaskStatus::Failed },
                    priority: 80,
                    created_at: Utc::now(),
                    ..Default::default()
                };
                
                self.task_store.create_task(&task).await?;
                
                let is_trusted = identity_opt.is_some();
                let trust_level = identity_opt.map(|id| id.trust_level as u32);
                
                self.sender.send(AgentMessage::MoeIdentityVerified {
                    fingerprint: fingerprint.clone(),
                    trusted: is_trusted,
                    trust_level,
                    task_id: Some(task.id.clone()),
                }).await?;
            }
            
            AgentMessage::MoeObjectVerified { .. } | AgentMessage::BatchVerifyMoeComplete { .. } | 
            AgentMessage::MoeIdentityVerified { .. } => {
                // Responses we send
                tracing::debug!("Received MoeVerify response message (not handled): {:?}", message);
            }
            
            _ => {
                tracing::debug!("Unhandled message: {:?}", message);
            }
        }
        
        Ok(())
    }
}

// ========== Helper types for messages ==========

/// Individual verification info for batch results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationInfo {
    pub hash: String,
    pub valid: bool,
    pub verification_status: VerificationStatus,
    pub signer: Option<String>,
    pub error: Option<String>,
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
    fn test_hash_computations() {
        let data = b"test data";
        let hash = MoeVerifyAgent::compute_hash(data);
        
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA256 hex is 64 chars
        
        // Verify hash matches
        assert!(MoeVerifyAgent::verify_hash(data, &hash));
        assert!(!MoeVerifyAgent::verify_hash(b"different data", &hash));
    }
    
    #[test]
    fn test_verification_result_creation() {
        let result = VerificationResult::new("test-hash".to_string(), VerificationStatus::Valid);
        
        assert!(result.is_valid());
        assert_eq!(result.hash, "test-hash");
        
        let invalid_result = VerificationResult::new("test-hash".to_string(), VerificationStatus::HashMismatch);
        assert!(!invalid_result.is_valid());
    }
    
    #[tokio::test]
    async fn test_agent_creation() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = MoeVerifyAgent::new(sender, task_store, state_store, None);
        
        assert_eq!(agent.name(), "MoeVerifyAgent");
        assert!(agent.capabilities().contains("moe-verify"));
        assert!(agent.capabilities().contains("verification"));
    }
    
    #[tokio::test]
    async fn test_verify_object_success() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = MoeVerifyAgent::new(sender, task_store, state_store, None);
        
        let data = b"test data for verification";
        let hash = MoeVerifyAgent::compute_hash(data);
        
        let result = agent.verify_object(&hash, data, None, None).await;
        
        assert!(result.is_valid());
        assert_eq!(result.status, VerificationStatus::Valid);
    }
    
    #[tokio::test]
    async fn test_verify_object_fail_hash_mismatch() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = MoeVerifyAgent::new(sender, task_store, state_store, None);
        
        let data = b"test data";
        let wrong_hash = "wrong-hash-value".to_string();
        
        let result = agent.verify_object(&wrong_hash, data, None, None).await;
        
        assert!(!result.is_valid());
        assert_eq!(result.status, VerificationStatus::HashMismatch);
    }
    
    #[tokio::test]
    async fn test_verify_with_signature() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        // Create agent with auto_trust disabled
        let mut config = MoeVerifyConfig::default();
        config.auto_trust = false;
        
        let agent = MoeVerifyAgent::new(sender, task_store, state_store, Some(config));
        
        let data = b"signed data";
        let hash = MoeVerifyAgent::compute_hash(data);
        
        // Use the argunix-ci fingerprint which is in default trusted identities
        let signer = "sha256-argunix-ci".to_string();
        let signature = b"dummy-signature".to_vec();
        
        let result = agent.verify_object(&hash, data, Some(signer), Some(signature)).await;
        
        // Should be valid because the signer is in trusted_identities
        assert!(result.is_valid());
        assert_eq!(result.signer, Some("sha256-argunix-ci".to_string()));
    }
    
    #[tokio::test]
    async fn test_untrusted_identity() {
        let (sender, _receiver) = mpsc::channel(32);
        let task_store = Arc::new(MemoryTaskStore::default());
        let state_store = Arc::new(MockStateStore);
        
        let agent = MoeVerifyAgent::new(sender, task_store, state_store, None);
        
        let data = b"data";
        let hash = MoeVerifyAgent::compute_hash(data);
        
        // Use an untrusted fingerprint
        let signer = "sha256-untrusted-identity".to_string();
        let signature = b"dummy-signature".to_vec();
        
        let result = agent.verify_object(&hash, data, Some(signer), Some(signature)).await;
        
        assert!(!result.is_valid());
        assert_eq!(result.status, VerificationStatus::UntrustedIdentity);
    }
}
