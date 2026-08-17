# AgentFlow / TaskFleet: argunix + Mœ Sovereignty Integration

<!--
SPDX-FileCopyrightText: 2026 AgentFlow Contributors
SPDX-License-Identifier: Apache-2.0
-->

## Vision: Sovereign Agent-Driven CI/CD

A next-generation workflow system that unifies:

- **argunix's Nix-native CI** - Declarative builds, flake evaluation, safe PR handling
- **Mœ Sovereignty concepts** - Self-hosted, autonomous, multi-generational computing
- **AgentFlow/TaskFleet** - Intelligent agent orchestration, distributed task execution

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        AGENTFLOW / TASKFLEET                                 │
│                 Sovereign AI Agent Platform                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                    CONTROL PLANE (AI Agents)                       │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐               │   │
│  │  │  Planner    │  │  Scheduler  │  │  Orchestrtr │               │   │
│  │  │   Agent     │  │   Agent     │  │   Agent     │               │   │
│  │  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘               │   │
│  └─────────┼─────────────────┼─────────────────┼───────────────────────┘   │
│            │                 │                 │                            │
│            ▼                 ▼                 ▼                            │
│  ┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐          │
│  │   Task Pool     │ │   State Graph   │ │  Knowledge Base │          │
│  │   Management    │ │    (Mœ tout)    │ │    (Semantic)   │          │
│  └─────────────────┘ └─────────────────┘ └─────────────────┘          │
│            │                 │                 │                            │
│            └─────────────────┴─────────────────┘                            │
│                              │                                              │
│                              ▼                                              │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                    DATA PLANE (Multi-Generational)                   │   │
│  │                                                                       │   │
│  │  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐     │   │
│  │  │   build Gen-0   │  │   build Gen-1   │  │   build Gen-N   │     │   │
│  │  │  (Nix Store)    │  │  (Nix Store)    │  │  (Nix Store)    │     │   │
│  │  └────────┬────────┘  └────────┬────────┘  └────────┬────────┘     │   │
│  │           │                   │                   │               │   │
│  │           ▼                   ▼                   ▼               │   │
│  │  ┌─────────────────────────────────────────────────────────────┐   │   │
│  │  │                 Mœ Storage Layer (S3, IPFS, etc.)          │   │   │
│  │  └─────────────────────────────────────────────────────────────┘   │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │              EXECUTION PLANE (Sovereign Runners)                    │   │
│  │                                                                       │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌───────────────────────────┐   │   │
│  │  │ argunix     │  │  Mœ Node     │  │  AI Agent Runner          │   │   │
│  │  │  Builder    │  │  (Worker)    │  │  (Llama.cpp, etc.)        │   │   │
│  │  └─────────────┘  └─────────────┘  └───────────────────────────┘   │   │
│  │                                                                       │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Core Principles

### 1. From argunix: Nix-Native Intelligence

- **Flake-First**: Everything is a Nix flake
- **Declarative**: Configuration as code
- **Reproducible**: Deterministic builds
- **Safe**: Permission-gated third-party code
- **Efficient**: DAG-based scheduling, shared dependencies

### 2. From Mœ Sovereignty: Autonomous Computing

- **Self-Hosted**: Run anywhere, no cloud lock-in
- **Multi-Generational**: Builds evolve across generations
- **Sovereign Identity**: Each node has cryptographic identity
- **Zero-Trust**: No implicit permissions
- **Resilient**: Survives network partitions

### 3. From AgentFlow/TaskFleet: Intelligent Orchestration

- **Agent-Based**: Autonomous agents make decisions
- **Task-Driven**: Work decomposed into atomic tasks
- **Dynamic**: Agents adapt to changing conditions
- **Collaborative**: Agents coordinate and share knowledge
- **Observable**: Full transparency into all operations

## Component Design

### 1. Agent Types

#### Task Agents (from TaskFleet)
- **Planner Agent**: Breaks down workflows into tasks
- **Scheduler Agent**: Assigns tasks to appropriate runners
- **Monitor Agent**: Tracks task progress and health
- **Recovery Agent**: Handles failures and retries

#### CI Agents (from argunix concepts)
- **Flake Analyzer Agent**: Evaluates Nix flakes, discovers outputs
- **Dependency Graph Agent**: Builds and analyzes DAGs
- **Security Gate Agent**: Validates permissions, allowlists
- **Cache Manager Agent**: Manages binary cache, GC roots

#### Sovereignty Agents (from Mœ concepts)
- **Identity Agent**: Manages cryptographic identities
- **Storage Agent**: Handles multi-generational storage
- **Consensus Agent**: Manages distributed agreement
- **Discovery Agent**: Finds and registers nodes

### 2. Task Types

#### Nix Tasks (argunix-inspired)
```yaml
# nix-eval task
type: nix-eval
flake_url: https://github.com/org/repo
flake_ref: main
system: x86_64-linux
targets:
  - packages.default
  - checks.all

# nix-build task
type: nix-build
flake_url: https://github.com/org/repo
flake_ref: main
system: x86_64-linux
drv_path: /nix/store/...-package.drv
```

#### AI Tasks (AgentFlow)
```yaml
# ai-inference task
type: ai-inference
model: llama3.2:70b
prompt: "Analyze this Nix flake"
context:
  - flake_url: https://github.com/org/repo
  - system: x86_64-linux

# ai-code-review task
type: ai-code-review
handbook: SECURITY_POLICY.md
code_path: /path/to/changes
action: review
```

#### Sovereignty Tasks (Mœ-inspired)
```yaml
# moe-sync task
type: moe-sync
generation: 0
source: /nix/store/...-gen0
target: /nix/store/...-gen1

# moe-verify task
type: moe-verify
integrity: sha256:...
signature: ed25519:...

# moe-gc task
type: moe-gc
retention_policy: "30d"
storage_class: hot
```

### 3. Knowledge Graph (Mœ tout + argunix metadata)

The knowledge graph combines:

```
┌─────────────────────────────────────────────────────────────────┐
│                        KNOWLEDGE GRAPH                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────────┐    ┌─────────────────┐    ┌─────────────┐ │
│  │   Nix Graph     │    │   Build Graph   │    │  Agent      │ │
│  │  (Dependencies) │    │  (Executions)   │    │  Knowledge  │ │
│  └────────┬────────┘    └────────┬────────┘    └──────┬──────┘ │
│           │                        │                     │        │
│           │                        ▼                     │        │
│           │              ┌─────────────────┐               │        │
│           └─────────────►│    Unified      │◄──────────────┘        │
│                          │   Query Engine  │                         │
│                          └─────────────────┘                         │
│                                                                  │
│  Query Examples:                                                      │
│    - "Find all builds depending on glibc"                             │
│    - "Show me failed builds in the last 24h"                          │
│    - "What agents worked on this PR?"                                │
│    - "List all generations of this derivation"                       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### 4. Storage Architecture (Multi-Generational)

```
┌─────────────────────────────────────────────────────────────────┐
│                    STORAGE LAYERS                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌───────────────────────────────────────────────────────────┐ │
│  │                    Generation 0 (Current)                   │ │
│  │   /nix/store/*.drv, *.narinfo, *.tar.gz                     │ │
│  │   - Live build artifacts                                      │ │
│  │   - Active references                                         │ │
│  └───────────────────────────────────────────────────────────┘ │
│                                    │                                │
│                                    ▼                                │
│  ┌───────────────────────────────────────────────────────────┐ │
│  │                    Generation 1                             │ │
│  │   /nix/store/gen1/*.drv, *.narinfo, *.tar.gz                │ │
│  │   - Previous generation builds                                │ │
│  │   - Rollback capability                                       │ │
│  └───────────────────────────────────────────────────────────┘ │
│                                    │                                │
│                                    ▼                                │
│  ┌───────────────────────────────────────────────────────────┐ │
│  │                    Generation N                             │ │
│  │   /nix/store/genN/*.drv                                      │ │
│  │   - Historical builds                                         │ │
│  │   - Audit trail                                               │ │
│  └───────────────────────────────────────────────────────────┘ │
│                                    │                                │
│                                    ▼                                │
│  ┌───────────────────────────────────────────────────────────┐ │
│  │                    Mœ Storage Backend                       │ │
│  │   - Content-addressable (IPFS, S3, local)                    │ │
│  │   - Deduplicated                                              │ │
│  │   - Encrypted (optional)                                      │ │
│  │   - Signed                                                    │ │
│  └───────────────────────────────────────────────────────────┘ │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### 5. Agent Orchestration Workflow

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Trigger    │────►│   Intake    │────►│   Analyze   │────►│   Plan      │
│  (Webhook)   │     │   Agent     │     │   Agent     │     │   Agent     │
└─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘
          │                  │                  │                  │
          ▼                  ▼                  ▼                  ▼
┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│ Webhook     │     │ Normalize   │     │ Flake       │     │ Create      │
│ Event       │     │ Event       │     │ Analysis    │     │ Task Graph  │
│             │     │             │     │             │     │             │
└─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘
          │                  │                  │                  │
          └──────────────────┴──────────────────┴──────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            SCHEDULER AGENT                                    │
│ aimer: Assign tasks to runners based on:                                     │
│   - Task requirements (system, dependencies)                                  │
│   - Runner capabilities (hardware, available packages)                        │
│   - Priority (urgent PRs, scheduled builds)                                   │
│   - Sovereignty constraints (data locality, compliance)                       │
└─────────────────────────────────────────────────────────────────────────────┘
          │                          │                          │
┌─────────▼─────────┐    ┌─────────▼─────────┐    ┌─────────▼─────────┐
│  Nix Runner      │    │  AI Runner        │    │  Mœ Runner        │
│  (argunix-based)  │    │  (Llama.cpp)      │    │  (Sovereign)     │
└────────┬────────┘    └────────┬────────┘    └────────┬────────┘
         │                      │                      │
         ▼                      ▼                      ▼
┌─────────────────────────────────────────────────────────────────────┐
│                        RESULT AGGREGATION                            │
│  - Collect build artifacts                                           │
│  - Aggregate logs and metrics                                         │
│  - Verify signatures                                                  │
│  - Update knowledge graph                                             │
│  - Post status updates                                                │
└─────────────────────────────────────────────────────────────────────┘
```

## Implementation: TaskFleet Integration

### 1. Flake Schema

```nix
# flake.nix for AgentFlow / TaskFleet
{
  description = "Sovereign Agent-Driven CI/CD Platform";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    argunix.url = "github:tobias-weiss-ai-xr/argunix";
    moe.url = "github:nix-community/moe";  # or your Mœ implementation
    srvos.url = "github:nix-community/srvos";  # ServiceOS for agents
    taskfleet.url = "github:nix-community/taskfleet";
  };

  outputs = {
    self,
    nixpkgs,
    argunix,
    moe,
    srvos,
    taskfleet,
    ...
  } @ inputs:
    let
      systems = [ "x86_64-linux" "aarch64-darwin" "aarch64-linux" ];
      pkgs = import nixpkgs { system = "x86_64-linux"; };
    in {
      # NixOS modules for the platform
      nixosModules = {
        agentflow = ./modules/agentflow;
        taskfleet = ./modules/taskfleet;
        argunix-engine = ./modules/argunix-engine;
        moe-storage = ./modules/moe-storage;
      };

      # Service packages
      packages = nixpkgs.lib.genAttrs systems (system: {
        agentflow = import ./. { inherit system inputs; };
        taskfleet-server = inputs.taskfleet.packages.${system}.server;
        taskfleet-worker = inputs.taskfleet.packages.${system}.worker;
      });

      # Dev shells
      devShells = nixpkgs.lib.genAttrs systems (system: {
        default = pkgs.mkShell {
          buildInputs = [
            (import.fromTOML ./taskfleet/Cargo.toml).packages.taskfleet-server
            (pkgs.callPackage ./argunix-engine {})
            pkgs.llama-cpp
            pkgs.nix
          ];
        };
      });
    };
}
```

### 2. Agent Definitions (Rust)

```rust
// src/agents/mod.rs

pub mod planner;
pub mod scheduler;
pub mod orchestrator;
pub mod analyzer;
pub mod security;
pub mod storage;
pub mod consensus;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::mpsc;

/// Agent types in the system
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentType {
    // TaskFleet agents
    Planner,
    Scheduler,
    Monitor,
    Recovery,
    
    // argunix-inspired agents
    FlakeAnalyzer,
    DependencyGraph,
    SecurityGate,
    CacheManager,
    Builder,
    
    // Mœ Sovereignty agents
    IdentityManager,
    StorageManager,
    ConsensusManager,
    Discovery,
    
    // AI agents
    AICodeReviewer,
    AIPlanner,
    AIQualityGate,
}

/// Agent message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessage {
    /// Start a new task
    StartTask { task: TaskDefinition },
    
    /// Task completed
    TaskComplete { task_id: String, result: TaskResult },
    
    /// Task failed
    TaskFailed { task_id: String, error: String },
    
    /// Query knowledge graph
    QueryGraph { query: GraphQuery },
    
    /// Update knowledge graph
    UpdateGraph { update: GraphUpdate },
    
    /// Sync storage generation
    SyncGeneration { gen: u64, data: Vec<u8> },
}

/// Agent trait
#[async_trait]
pub trait Agent: Send + Sync {
    fn agent_type(&self) -> AgentType;
    fn name(&self) -> &str;
    
    async fn handle_message(
        &self,
        message: AgentMessage,
        sender: mpsc::Sender<AgentMessage>,
        state: &mut AgentState,
    ) -> Result<()>;
    
    async fn on_start(&mut self, state: &mut AgentState) -> Result<()>;
    async fn on_shutdown(&mut self) -> Result<()>;
}
```

### 3. Task Definition

```rust
// src/tasks/mod.rs

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskType {
    // Nix tasks (argunix-inspired)
    NixEval,
    NixBuild,
    NixCheck,
    NixDevShell,
    NixBundle,
    
    // AI tasks
    AIFlakeAnalysis,
    AICodeReview,
    AIPlanGeneration,
    AIQualityCheck,
    
    // Mœ tasks
    MoeSync,
    MoeVerify,
    MoeGC,
    MoeRestore,
    
    // General
    CustomCommand,
    MultiTask,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDefinition {
    pub id: String,
    pub task_type: TaskType,
    
    // Nix-specific
    pub flake_url: Option<String>,
    pub flake_ref: Option<String>,
    pub system: Option<String>,
    pub targets: Option<Vec<String>>,
    pub drv_path: Option<String>,
    
    // AI-specific
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub handbook: Option<String>,
    
    // Mœ-specific
    pub generation: Option<u64>,
    pub storage_class: Option<String>,
    pub retention_policy: Option<String>,
    
    // requirements
    pub requires: Vec<String>,
    pub priority: u32,
    pub timeout: Option<u64>,
    pub retry_policy: Option<RetryPolicy>,
    
    // Sovereignty constraints
    pub data_locality: Option<DataLocality>,
    pub compliance_tags: Option<Vec<String>>,
    pub cryptographic_proof: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataLocality {
    Anywhere,
    Region(String),
    Zone(String),
    Node(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_seconds: u64,
    pub backoff_multiplier: f32,
}
```

### 4. Sovereign Identity System (Mœ-inspired)

```rust
// src/sovereignty/identity.rs

use ed25519_dalek::{Signer, Verifier, Signature, SigningKey, VerifyingKey};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

/// Cryptographic identity for agents and nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SovereignIdentity {
    /// Public key (identity)
    pub public_key: String,
    
    /// Key fingerprint
    pub fingerprint: String,
    
    /// Node name
    pub name: String,
    
    /// Node type (agent, builder, storage, etc.)
    pub node_type: String,
    
    /// Capabilities
    pub capabilities: Vec<String>,
    
    /// Generation number (Mœ multi-generational)
    pub generation: u64,
    
    /// Creation timestamp
    pub created_at: i64,
    
    /// Expiration timestamp
    pub expires_at: Option<i64>,
}

/// Trust registry
#[derive(Debug, Clone, Default)]
pub struct TrustRegistry {
    identities: HashMap<String, SovereignIdentity>, // fingerprint -> identity
    revocation_list: Vec<String>, // revoked fingerprints
}

impl TrustRegistry {
    /// Register a new identity (with proof of work or manual approval)
    pub fn register_identity(
        &mut self,
        identity: SovereignIdentity,
        signature: &Signature,
        challenge: &[u8],
    ) -> Result<(), TrustError> {
        // Verify the identity signed the challenge
        let verifying_key = VerifyingKey::from_base64_string(&identity.public_key)?;
        verifying_key.verify(challenge, signature)?;
        
        // Check not revoked
        if self.revocation_list.contains(&identity.fingerprint) {
            return Err(TrustError::Revoked);
        }
        
        self.identities.insert(identity.fingerprint.clone(), identity);
        Ok(())
    }
    
    /// Check if identity is trusted
    pub fn is_trusted(&self, fingerprint: &str) -> bool {
        self.identities.contains_key(fingerprint) 
            && !self.revocation_list.contains(&fingerprint.to_string())
    }
    
    /// Revoke an identity
    pub fn revoke(&mut self, fingerprint: &str) -> bool {
        if self.identities.remove(fingerprint).is_some() {
            self.revocation_list.push(fingerprint.to_string());
            return true;
        }
        false
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("Invalid signature")]
    InvalidSignature,
    #[error("Identity revoked")]
    Revoked,
    #[error("Identity already exists")]
    AlreadyExists,
}
```

### 5. Multi-Generational Storage (Mœ-inspired)

```rust
// src/sovereignty/storage.rs

use std::path::{Path, PathBuf};
use std::collections::BTreeMap;
use serde::{Serialize, Deserialize};
use sha2::{Sha256, Digest};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Multi-generational storage backend
#[derive(Debug, Clone)]
pub struct MoeStorage {
    /// Base directory for all generations
    base_dir: PathBuf,
    
    /// Current generation
    current_generation: u64,
    
    /// Max generations to keep
    max_generations: u64,
    
    /// Storage backends (local, S3, IPFS)
    backends: Vec<StorageBackend>,
}

/// Storage backend types
#[derive(Debug, Clone)]
pub enum StorageBackend {
    Local { path: PathBuf },
    S3 { 
        bucket: String,
        region: String,
        credentials: S3Credentials,
    },
    IPFS { 
        gateway: String,
        private_network: bool,
    },
    NixStore { store_path: PathBuf },
}

/// Content-addressed object
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageObject {
    /// SHA256 hash (content address)
    pub hash: String,
    
    /// Size in bytes
    pub size: u64,
    
    /// Generation when created
    pub generation: u64,
    
    /// Content type
    pub content_type: String,
    
    /// Metadata
    pub metadata: BTreeMap<String, String>,
    
    /// Signature (optional)
    pub signature: Option<String>,
}

impl MoeStorage {
    /// Store an object in the current generation
    pub async fn store(
        &self,
        data: &[u8],
        content_type: &str,
        metadata: BTreeMap<String, String>,
    ) -> Result<StorageObject, StorageError> {
        // Calculate hash
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash = format!("{:x}", hasher.finalize());
        
        let obj = StorageObject {
            hash: hash.clone(),
            size: data.len() as u64,
            generation: self.current_generation,
            content_type: content_type.to_string(),
            metadata,
            signature: None,
        };
        
        // Store in all backends
        for backend in &self.backends {
            self.store_in_backend(backend, &hash, data).await?;
        }
        
        Ok(obj)
    }
    
    /// Load an object from any generation
    pub async fn load(
        &self,
        hash: &str,
        generation: Option<u64>,
    ) -> Result<Vec<u8>, StorageError> {
        let gen = generation.unwrap_or(self.current_generation);
        
        // Try each backend
        for backend in &self.backends {
            if let Ok(data) = self.load_from_backend(backend, hash, gen).await {
                return Ok(data);
            }
        }
        
        Err(StorageError::NotFound(hash.to_string()))
    }
    
    /// Sync to next generation
    pub async fn next_generation(&mut self) -> Result<(), StorageError> {
        self.current_generation += 1;
        
        // Archive previous generation
        self.archive_generation(self.current_generation - 1).await?;
        
        // Cleanup old generations if needed
        if self.current_generation > self.max_generations {
            let old_gen = self.current_generation - self.max_generations;
            self.cleanup_generation(old_gen).await?;
        }
        
        Ok(())
    }
    
    /// Archive a generation
    pub async fn archive_generation(&self, generation: u64) -> Result<(), StorageError> {
        // Create archive manifest
        let manifest = self Collect objects from this generation
        // Store manifest in cold storage
        // Done
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("Object not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Storage backend error")]
    BackendError,
}
```

## Integration Scenarios

### Scenario 1: AI-Assisted Nix CI

```yaml
# workflow.yaml
name: nix-ci-with-ai-review

triggers:
  - on: push
    branches: [main]
    repos: [opendesk-edu/opendesk-nix]
  - on: pull_request
    repos: [opendesk-edu/opendesk-nix]

workflow:
  - name: validate-flake
    agent: flake-analyzer
    task: NixEval
    params:
      flake_url: "{{github.event.repository.url}}"
      flake_ref: "{{github.event.after}}"
      targets: ["packages.x86_64-linux.default"]
    
  - name: ai-review
    agent: ai-code-reviewer
    task: AICodeReview
    depends_on: [validate-flake]
    params:
      handbook: SECURITY_POLICY.md
      code_path: "."
      model: llama3.2:70b
      
  - name: build-packages
    agent: scheduler
    task: MultiTask
    depends_on: [ai-review]
    if: "{{steps.ai-review.outputs.approved == true}}"
    parallel: true
    strategy:
      matrix:
        system: [x86_64-linux, aarch64-linux, x86_64-darwin]
    params:
      flake_url: "{{github.event.repository.url}}"
      flake_ref: "{{github.event.after}}"
      targets: ["packages.{{matrix.system}}.*"]
    
  - name: cache-push
    agent: cache-manager
    task: NixCopy
    depends_on: [build-packages]
    params:
      from: "${{steps.build-packages.outputs.store_path}}"
      to: "s3://opendesk-cache/main"
      
  - name: deploy-staging
    agent: moe-storage
    task: MoeSync
    if: "{{github.event_name == 'push' && github.ref == 'refs/heads/main'}}"
    params:
      generation: 0
      target: "staging"
```

### Scenario 2: Sovereign Multi-Node Build Farm

```yaml
# build-farm.yaml
name: sovereign-build-farm

agents:
  - id: planner-node-1
    type: Planner
    capabilities: [task-planning, dag-building]
    identity: "ed25519:abc123..."
    
  - id: scheduler-node-1
    type: Scheduler
    capabilities: [task-scheduling, load-balancing]
    identity: "ed25519:def456..."
    
  - id: builder-x86_64-01
    type: Builder
    capabilities: [nix-build, x86_64-linux]
    identity: "ed25519:ghi789..."
    resources:
      cpu: 8
      memory: 32GB
      storage: 1TB
    
  - id: builder-aarch64-01
    type: Builder
    capabilities: [nix-build, aarch64-linux]
    identity: "ed25519:jkl012..."
    resources:
      cpu: 8
      memory: 32GB
      
  - id: ai-reviewer-01
    type: AICodeReviewer
    capabilities: [ai-inference, code-review]
    identity: "ed25519:mno345..."
    resources:
      gpu: 1
      memory: 24GB

trust:
  # Mutual TLS / ed25519 signing
  ca_certificate: "..."/
  node_identities:
    - fingerprint: "abc123..."
      name: planner-node-1
      trusted: true
    - fingerprint: "def456..."
      name: scheduler-node-1
      trusted: true

storage:
  generations:
    max: 10
    current: 0
    
  backends:
    - type: local
      path: /var/lib/agentflow/storage
      
    - type: s3
      bucket: agentflow-builds
      region: eu-central-1
      
    - type: ipfs
      gateway: https://ipfs.opendesk.example.com
      private: true

sovereignty:
  # Data locality constraints
  data_locality:
    - regex: "eu-.*"
      regions: ["eu-central-1", "eu-west-1"]
      
    - regex: ".*secret.*"
      allowed_nodes: ["security-node-1"]
      
  # Compliance tags
  compliance:
    - tag: "gdp-pr"
      retain_for: 90d
      encrypt: true
      
    - tag: "public"
      retain_for: 30d
      encrypt: false
```

### Scenario 3: Git Flow with AI & Sovereign Builds

```
┌─────────┐     ┌─────────┐     ┌─────────┐     ┌─────────┐
│         │     │         │     │         │     │         │
│  Git    │────►│ Intake  │────►│  AI     │     │         │
│ Push    │     │ Agent   │     │ Review  │     │         │
│         │     │         │     │ Agent   │     │         │
└─────────┘     └─────────┘     └────┬────┘     │         │
                                        │          │         │
                                        ▼          │         │
┌─────────────────────────────────────────────────┐         │         │
│              PLANNING PHASE                        │         │         │
│                                                   │         │         │
│  ┌─────────┐     ┌─────────┐     ┌─────────┐  │         │         │
│  │ Flake   │◄───►│  Task   │◄───►│uksi    │  │         │         │
│  │ Analyzer│     │ Planner │     │ Orch.   │  │         │         │
│  └─────────┘     └─────────┘     └─────────┘  │         │         │
└─────────────────────────────────────────────────┘         │         │
          │                     │                     │            │         │
          ▼                     ▼                     ▼            │         │
┌──────────────────────────────────────────────────────────────┐    │         │
│                        TASK QUEUE                                 │    │         │
│  [Task A] [Task B] [Task C] ... [Task N]                          │    │         │
└──────────────────────────────────────────────────────────────┘    │         │
          │                      │                      │              │         │
┌─────────▼─────────┐    ┌─────────▼─────────┐    ┌─────────▼────────┐ │         │
│  Sovereign        │    │  Nix Build        │    │  AI Analysis     │ │         │
│  Builder Node 1   │    │  Runner           │    │  Runner          │ │         │
│  (x86_64)         │    │  (argunix-based)  │    │  (Llama.cpp)     │ │         │
└────────┬────────┘    └────────┬────────┘    └────────┬────────┘ │         │
         │                     │                     │            │         │
         ▼                     ▼                     ▼            ▼         ▼
┌─────────────────────────────────────────────────────────────────────┐
│                        MŒ STORAGE                                 │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                  │
│  │  Gen 0      │  │  Gen 1      │  │  Gen N      │                  │
│  │  (Current)   │  │  (Prev)     │  │  (History)  │                  │
│  └─────────────┘  └─────────────┘  └─────────────┘                  │
│                                                                  │
│  Backends: S3, IPFS, Local                                       │
└─────────────────────────────────────────────────────────────────────┘
          │                     │                     │
          ├─────────────────────┼─────────────────────┤
          ▼                     ▼                     ▼
┌─────────────────────────────────────────────────────────────────────┐
│                       RESULT & FEEDBACK                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                  │
│  │ Build Logs  │  │  AI Report  │  │  Status     │                  │
│  │ & Artifacts │  │  & Suggest. │  │  Updates    │                  │
│  └─────────────┘  └─────────────┘  └─────────────┘                  │
└─────────────────────────────────────────────────────────────────────┘
```

## NixOS Integration

### Flake Module

```nix
# modules/agentflow/default.nix
{ config, pkgs, lib, ... }:

let
  cfg = config.services.agentflow;
  moeCfg = config.services.moe-sovereignty;
  
  agentflowPackage = pkgs.callPackage (builtins.fetchGit {
    url = "https://github.com/tobias-weiss-ai-xr/agentflow";
    ref = "refs/tags/v0.1.0";
    sha256 = "0000000000000000000000000000000000000000000000000000";
  }) {};

in {
  options.services.agentflow = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable AgentFlow sovereign CI/CD platform";
    };
    
    agents = lib.mkOption {
      type = lib.types.listOf (lib.types.submodule {
        options = {
          name = lib.mkOption {
            type = lib.types.str;
            description = "Agent name";
          };
          type = lib.mkOption {
            type = lib.types.enum [
              "planner" "scheduler" "orchestrator" 
              "flake-analyzer" "builder" "ai-reviewer"
              "cache-manager" "storage-manager"
            ];
            description = "Agent type";
          };
          capabilities = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [];
            description = "Agent capabilities";
          };
        };
      });
      default = [];
      description = "List of agents to deploy";
    };
    
    sovereignty = lib.mkOption {
      type = lib.types.submodule {
        options = {
          enable = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Enable Mœ sovereignty features";
          };
          
          generations = lib.mkOption {
            type = lib.types.int;
            default = 10;
            description = "Maximum number of generations to retain";
          };
          
          trustRegistry = lib.mkOption {
            type = lib.types.path;
            default = "/var/lib/agentflow/trust-registry.json";
            description = "Path to trust registry";
          };
        };
      };
      default = {};
      description = "Mœ sovereignty configuration";
    };
    
    storage = lib.mkOption {
      type = lib.types.submodule {
        options = {
          backends = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ "local" "s3" ];
            description = "Storage backends to use";
          };
          
          s3 = lib.mkOption {
            type = lib.types.nullOr (lib.types.submodule {
              options = {
                bucket = lib.mkOption {
                  type = lib.types.str;
                  description = "S3 bucket name";
                };
                region = lib.mkOption {
                  type = lib.types.str;
                  default = "eu-central-1";
                  description = "S3 region";
                };
              };
            });
            default = null;
            description = "S3 storage configuration";
          };
        };
      };
      default = {};
      description = "Storage configuration";
    };
  };
  
  config = lib.mkIf cfg.enable {
    # creates users/groups + required system packages
    users.groups.agentflow = {
      gid = 30000;
    };
    
    users.users.agentflow = {
      uid = 30000;
      gid = 30000;
      group = "agentflow";
      home = "/var/lib/agentflow";
      systemPackages = with pkgs; [
        bash coreutils findutils gnused procps
        ${agentflowPackage}
      ];
    };
    
    systemd.services.agentflow = {
      description = "AgentFlow Sovereign CI/CD Platform";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" "nix-daemon.target" ];
      
      serviceConfig = {
        User = "agentflow";
        Group = "agentflow";
        WorkingDirectory = "/var/lib/agentflow";
        ExecStart = "${agentflowPackage}/bin/agentflow-server --config /etc/agentflow/config.yaml";
        Restart = "on-failure";
        RestartSec = "5s";
        Environment = [
          "RUST_LOG=info"
          "AGENTFLOW_CONFIG=/etc/agentflow/config.yaml"
        ];
      };
    };
    
    systemd.services.agentflow-worker = lib.forEach cfg.agents (agent: {
      description = "AgentFlow Worker - ${agent.name}";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" "agentflow.service" ];
      requires = [ "agentflow.service" ];
      
      serviceConfig = {
        User = "agentflow";
        Group = "agentflow";
        WorkingDirectory = "/var/lib/agentflow";
        ExecStart = "${agentflowPackage}/bin/agentflow-worker --agent ${agent.name} --type ${agent.type}";
        Restart = "on-failure";
        RestartSec = "5s";
      };
    });
    
    # Create directories
    system.activationScripts.setupAgentflow = lib.mkBefore ''
      mkdir -p /var/lib/agentflow/{storage,trust,logs,secrets}
      chown -R agentflow:agentflow /var/lib/agentflow
      chmod 700 /var/lib/agentflow/secrets
    '';
    
    # Generate configuration
    systemd.services.agentflow.serviceConfig.Environment = [
      "AGENTFLOW_CONFIG=${config.age.secrets.agentflow.path}"
    ];
    
    # Firewall
    networking.firewall.allowedTCPPorts = [
      { port = 8080; description = "AgentFlow HTTP API"; }
      { port = 8081; description = "AgentFlow metrics"; }
    ];
    
    # If Mœ sovereignty is enabled
    services.moe-sovereignty = lib.mkIf moeCfg.enable {
      enable = true;
      integration.agentflow = true;
    };
  };
}
```

### Service Integration

```nix
# modules/argunix-engine.nix
{ config, pkgs, lib, ... }:

let
  cfg = config.services.argunix-engine;

in {
  options.services.argunix-engine = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable argunix engine for AgentFlow";
    };
    
    flakeAllowlist = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = "Allowed flake URLs for evaluation";
      example = [ "github:opendesk-edu/*" "github:nix-community/*" ];
    };
    
    builderEnrollment = lib.mkOption {
      type = lib.types.submodule {
        options = {
          enable = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Enable builder enrollment";
          };
          
          token = lib.mkOption {
            type = lib.types.str;
            description = "Builder enrollment token";
          };
          
          listenPort = lib.mkOption {
            type = lib.types.port;
            default = 45678;
            description = "Port for builder enrollment";
          };
        };
      };
      default = {};
    };
  };
  
  config = lib.mkIf cfg.enable {
    # Import argunix as a library
    nixpkgs.overlays = [
      (self: super: {
        argunix = super.callPackage (builtins.fetchGit {
          url = "https://github.com/tobias-weiss-ai-xr/argunix";
          ref = "refs/tags/v0.0.1-dev";
          sha256 = "0000000000000000000000000000000000000000000000000000";
        }) {};
      })
    ];
    
    # argunix-engine service
    systemd.services.argunix-engine = {
      description = "argunix Engine for AgentFlow";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" "agentflow.service" ];
      requires = [ "agentflow.service" ];
      
      serviceConfig = {
        User = "agentflow";
        Group = "agentflow";
        ExecStart = ''
          ${pkgs.argunix}/bin/argunixd \
            --config /var/lib/agentflow/argunix-config.yaml
        '';
        Restart = "on-failure";
        RestartSec = "5s";
        Environment = [
          "RUST_LOG=info"
          "ARGUNIX_CONFIG=/var/lib/agentflow/argunix-config.yaml"
        ];
      };
    };
    
    # Generate configuration
    system.activationScripts.configArgunix = lib.mkAfter ''
      ${pkgs.writeText "argunix-config" ''
        external_url: "https://ci.${config.networking.hostName}"
        listen: "0.0.0.0:8080"
        database:
          type: sqlite
          path: /var/lib/agentflow/argunix.sqlite
        builder_enrollment:
          ${lib.optionalString (cfg.builderEnrollment.enable) ''
            enabled: true
            listen: 0.0.0.0:${toString cfg.builderEnrollment.listenPort}
            enrollment_token: "${cfg.builderEnrollment.token}"
          ''}
        forges: {}
        allowlist:
          ${lib.concatStringsSep "\n" (map (x: "  - ${x}") cfg.flakeAllowlist)}
      ''} > /var/lib/agentflow/argunix-config.yaml
      chown agentflow:agentflow /var/lib/agentflow/argunix-config.yaml
    '';
  };
}
```

## API Design

### REST API (HTTP/JSON)

```
GET   /api/v1/agents           - List all agents
POST  /api/v1/agents           - Register new agent
GET   /api/v1/agents/{id}      - Get agent details
PUT   /api/v1/agents/{id}      - Update agent
DELETE /api/v1/agents/{id}     - Remove agent

GET   /api/v1/tasks            - List all tasks
POST  /api/v1/tasks            - Submit new task
GET   /api/v1/tasks/{id}       - Get task details
GET   /api/v1/tasks/{id}/logs  - Get task logs
DELETE /api/v1/tasks/{id}      - Cancel task

GET   /api/v1/workflows        - List workflows
POST  /api/v1/workflows        - Create workflow
GET   /api/v1/workflows/{id}   - Get workflow details

GET   /api/v1/storage          - List storage objects
GET   /api/v1/storage/{hash}   - Get storage object
POST  /api/v1/storage          - Store object

GET   /api/v1/generations      - List generations
POST  /api/v1/generations/next - Advance to next generation

GET   /api/v1/trust            - List trusted identities
POST  /api/v1/trust            - Add trusted identity
DELETE /api/v1/trust/{fingerprint} - Revoke identity

GET   /api/v1/metrics          - System metrics
GET   /api/v1/health           - Health check
```

### gRPC API (Protocol Buffers)

For high-performance agent-to-agent communication:

```protobuf
syntax = "proto3";

package agentflow.v1;

service AgentCommunication {
  rpc Register(RegisterRequest) returns (RegisterResponse);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  rpc AssignTask(AssignTaskRequest) returns (TaskAssignment);
  rpc TaskStatus(TaskStatusRequest) returns (TaskStatusResponse);
  rpc QueryKnowledge(KnowledgeQuery) returns (KnowledgeResponse);
  rpc SyncGeneration(SyncRequest) returns (SyncResponse);
  
  // Streaming RPCs
  rpc TaskLogs(TaskLogsRequest) returns (stream LogEntry);
  rpc EventStream(EventStreamRequest) returns (stream Event);
}

message RegisterRequest {
  string agent_id = 1;
  AgentType type = 2;
  string public_key = 3;
  string signature = 4;
  string challenge = 5;
  repeated string capabilities = 6;
  map<string, string> metadata = 7;
}

message TaskAssignment {
  string task_id = 1;
  TaskType task_type = 2;
  bytes payload = 3;
  int64 timeout_seconds = 4;
  int32 priority = 5;
}

message KnowledgeQuery {
  string query = 1;
  QueryType query_type = 2;
}

message SyncRequest {
  uint64 from_generation = 1;
  uint64 to_generation = 2;
  Filter filter = 3;
}
```

## Configuration File Format

```yaml
# agentflow.yaml
version: "1.0"

# System identity (Mœ sovereignty)
identity:
  name: "agentflow-main"
  public_key: "ed25519:abc123..."
  private_key_path: "/var/lib/agentflow/keys/private.pem"
  node_type: "control-plane"

# Control plane configuration
control_plane:
  host: "0.0.0.0"
  port: 8080
  tls:
    cert_file: "/var/lib/agentflow/certs/cert.pem"
    key_file: "/var/lib/agentflow/certs/key.pem"
  
  # Agent discovery
  discovery:
    method: "kubernetes"
    # or: method: "consul"
    # or: method: "static"
    static:
      agents:
        - host: "builder-1.example.com"
          port: 8081
          
# Storage configuration (Mœ multi-generational)
storage:
  base_dir: "/var/lib/agentflow/storage"
  
  generations:
    max: 10
    current: 0
    
  backends:
    - type: local
      path: "/var/lib/agentflow/storage/local"
      
    - type: s3
      bucket: "agentflow-builds"
      region: "eu-central-1"
      endpoint: "https://s3.opendesk.example.com"
      access_key: "${S3_ACCESS_KEY}"
      secret_key: "${S3_SECRET_KEY}"
      
    - type: ipfs
      gateway: "https://ipfs.agentflow.example.com"
      private_network: true
      
    - type: nix_store
      store_path: "/nix/store"

# Trust and security (Mœ sovereignty)
trust:
  ca_certificate: "${TRUST_CA_CERT}"
  
  # Trust on first use or explicit approval
  mode: "explicit"  # or "tofu"
  
  identities:
    - fingerprint: "ed25519:abc123..."
      name: "planner-1"
      trusted: true
      capabilities: ["task-planning"]
      
  revocation_list:
    - fingerprint: "ed25519:revoked..."
      reason: "Compromised"
      revoked_at: "2024-01-01T00:00:00Z"

# Agent definitions
agents:
  - name: "planner-main"
    type: "planner"
    capabilities: ["task-planning", "dag-building"]
    max_tasks: 100
    resources:
      cpu: 2
      memory: 4GB
    
  - name: "scheduler-main"
    type: "scheduler"
    capabilities: ["task-scheduling", "load-balancing"]
    max_tasks: 1000
    
  - name: "builder-x86_64-01"
    type: "builder"
    capabilities: ["nix-build", "x86_64-linux"]
    system: "x86_64-linux"
    nix:
      store_path: "/nix/store"
      trusted_users: ["agentflow"]
    resources:
      cpu: 8
      memory: 32GB
      storage: 1TB
    
  - name: "builder-aarch64-01"
    type: "builder"
    capabilities: ["nix-build", "aarch64-linux"]
    system: "aarch64-linux"
    
  - name: "ai-reviewer-01"
    type: "ai-reviewer"
    capabilities: ["ai-inference", "code-review"]
    ai:
      model: "llama3.2:70b"
      gpu: true
      gpu_memory: 24GB
    
  - name: "cache-manager-01"
    type: "cache-manager"
    capabilities: ["cache-management"]
    caches:
      - type: "s3"
        bucket: "agentflow-cache"
        
# Nix configuration (argunix-inspired)
nix:
  evaluators: 4
  builders: 10
  
  flake:
    # Auto-discover from repositories
    auto_discovery: true
    
    # Allowlist
    allowlist:
      - "github:opendesk-edu/*"
      - "github:nix-community/*"
      - "github:tobias-weiss-ai-xr/*"
    
    # Blocklist
    blocklist:
      - "github:malicious-actor/*"
    
  # Third-party PR handling
  third_party:
    enabled: true
    require_allowlist: true
    require_collaborator: true

# AI configuration
ai:
  enabled: true
  
  models:
    - name: "llama3.2:70b"
      url: "https://llama.cpp.example.com"
      
  handbooks:
    - name: "security-policy"
      path: "/var/lib/agentflow/handbooks/security.md"
      
    - name: "code-style"
      path: "/var/lib/agentflow/handbooks/style.md"

# Monitoring
monitoring:
  metrics:
    enabled: true
    port: 8081
    
  logging:
    level: "info"
    format: "json"
    
  tracing:
    enabled: true
    jaeger_endpoint: "http://jaeger:14268/api/traces"

# Sovereignty constraints
sovereignty:
  data_locality:
    - regex: "eu-.*"
      regions: ["eu-central-1", "eu-west-1"]
      
    - regex: ".*secret.*"
      allowed_nodes: ["security-node-1"]
      
  compliance:
    - tag: "gdp-pr"
      retain_for: 90d
      encrypt: true
      
    - tag: "public"
      retain_for: 30d
      encrypt: false
      
  # Node autonomy
  autonomy:
    enabled: true
    
    # Nodes can make local decisions
    local_decision_making: true
    
    # Conflict resolution
    conflict_resolution: "consensus"
```

## Knowledge Graph Schema

```graphql
# GraphQL Schema for AgentFlow Knowledge Graph

type Query {
  # Query agents
  agents(
    filter: AgentFilter
    limit: Int
    offset: Int
  ): [Agent!]!
  agent(id: ID!): Agent
  
  # Query tasks
  tasks(
    filter: TaskFilter
    limit: Int
    offset: Int
  ): [Task!]!
  task(id: ID!): Task
  
  # Query workflows
  workflows(
    filter: WorkflowFilter
    limit: Int
    offset: Int
  ): [Workflow!]!
  workflow(id: ID!): Workflow
  
  # Query storage
  storageObjects(
    filter: StorageObjectFilter
    limit: Int
    offset: Int
  ): [StorageObject!]!
  storageObject(hash: String!): StorageObject
  
  # Query builds
  builds(
    filter: BuildFilter
    limit: Int
    offset: Int
  ): [Build!]!
  build(id: ID!): Build
  
  # Query Nix graph
  nixGraph(
    flakeUrl: String!
    flakeRef: String
    system: String
  ): NixGraph
  
  # Query dependencies
  dependencies(
    package: String!
    system: String
  ): [Dependency!]!
  
  # Query security
  security(
    flakeUrl: String!
    flakeRef: String
  ): SecurityReport
}

type Mutation {
  # Create/Update agents
  createAgent(input: AgentInput!): Agent!
  updateAgent(id: ID!, input: AgentInput!): Agent!
  
  # Submit tasks
  submitTask(input: TaskInput!): Task!
  
  # Create workflows
  createWorkflow(input: WorkflowInput!): Workflow!
  
  # Store objects
  storeObject(input: StorageObjectInput!): StorageObject!
  
  # Manage trust
  trustIdentity(input: TrustInput!): Trust!
  revokeIdentity(fingerprint: String!): Trust!
  
  # Advance generations
  nextGeneration: Generation!
  
  # Sync storage
  syncStorage(input: SyncInput!): SyncResult!
}

type Subscription {
  # Real-time updates
  taskUpdates: Task!
  buildUpdates: Build!
  agentUpdates: Agent!
  
  # Event stream
  events: Event!
}

# Types
type Agent {
  id: ID!
  name: String!
  type: AgentType!
  status: AgentStatus!
  capabilities: [String!]!
  identity: Identity
  resources: ResourceSpec
  createdAt: DateTime!
  updatedAt: DateTime!
  lastSeen: DateTime
  tasksCompleted: Int!
  tasksFailed: Int!
}

type Task {
  id: ID!
  type: TaskType!
  status: TaskStatus!
  priority: Int!
  createdAt: DateTime!
  startedAt: DateTime
  completedAt: DateTime
  
  # Task-specific fields
  flakeUrl: String
  flakeRef: String
  system: String
  targets: [String!]
  drvPath: String
  
  # Execution
  assignedAgent: Agent
  logs: [LogEntry!]!
  artifacts: [Artifact!]!
  
  # DAG
  dependencies: [Task!]!
  dependents: [Task!]!
  
  # Error
  error: String
  exitCode: Int
}

type Build {
  id: ID!
  task: Task!
  storePath: String!
  derivation: String!
  outputs: [String!]!
  system: String!
  startedAt: DateTime!
  completedAt: DateTime
  duration: Float
  
  # Metadata from Nix
  nixVersion: String
  derivationName: String
  
  # Size
  size: Int
  
  # Sources
  flakeUrl: String
  flakeRef: String
}

type NixGraph {
  flakeUrl: String!
  flakeRef: String!
  system: String!
  
  # Graph structure
  nodes: [NixNode!]!
  edges: [NixEdge!]!
  
  # Analytics
  totalNodes: Int!
  totalEdges: Int!
  rootNodes: [NixNode!]!
  leafNodes: [NixNode!]!
}

type NixNode {
  id: ID!
  name: String!
  path: String
  type: NixNodeType!
  
  # Attributes
  attributes: [NixNodeAttribute!]!
  
  # Metadata
  description: String
  license: String
  maintainers: [String!]
}

type StorageObject {
  hash: String!
  size: Int!
  generation: Int!
  contentType: String!
  metadata: JSON
  createdAt: DateTime!
  
  # Locations
  locations: [ObjectLocation!]!
  
  # Integrity
  integrity: String
  signature: String
}

type Identity {
  fingerprint: String!
  publicKey: String!
  nodeType: String!
  name: String!
  trusted: Boolean!
  createdAt: DateTime!
  expiresAt: DateTime
  
  # Mœ plurality
  plurality: Int
  generation: Int
}

# Input types
type AgentInput {
  name: String!
  type: AgentType!
  capabilities: [String!]
  resources: ResourceSpecInput
}

type TaskInput {
  type: TaskType!
  priority: Int
  timeout: Int