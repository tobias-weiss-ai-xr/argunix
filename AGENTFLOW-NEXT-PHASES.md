# AgentFlow Next Phases - Implementation Roadmap

## Overview

This document outlines the next phases of AgentFlow development, building upon the completed core implementation.

## Completed (Phase 0)

- [x] Core type system (Agent, Task, Message, State)
- [x] In-memory message bus
- [x] 4 concrete agents (Planner, Scheduler, NixExecutor, FlakeAnalyzer)
- [x] CLI interface
- [x] Unit tests
- [x] GitHub repository setup

## Completed (Phase 1): Distributed Message Bus

- [x] Message bus abstraction (`agentflow-core/src/bus.rs`)
- [x] `MessageBus` trait with NATS and in-memory implementations
- [x] InMemoryBus with pub/sub support
- [x] NatsBus with NATS pub/sub (feature-gated)
- [x] Message serialization using bincode
- [x] MessageBusFactory trait for creating bus instances

## Completed (Phase 2): HTTP Gateway

- [x] Axum-based HTTP server (`agentflow-server` crate)
- [x] REST API endpoints:
  - GET/POST /api/v1/tasks - List and create tasks
  - GET/PATCH/DELETE /api/v1/tasks/{id} - Task management
  - GET /api/v1/agents - List agents
  - GET /api/v1/agents/{id} - Agent details
  - POST /api/v1/flakes/analyze - Analyze flakes
  - POST /api/v1/webhooks/* - GitHub/GitLab/Forgejo webhooks
- [x] Server configuration from environment variables
- [x] Error handling with ApiError type
- [x] Health check and metrics endpoints
- [x] API documentation endpoints

## Completed (Phase 2.5): Persistent Storage

- [x] Storage abstraction (`agentflow-storage` crate)
- [x] `TaskStore` and `StateStore` implementations:
  - In-memory storage (MemoryStorage)
  - Filesystem storage (FilesystemStorage)
  - SQLite storage stub (SqliteStorage)
  - Redis storage stub (RedisStorage)
- [x] StorageFactory trait for creating storage instances
- [x] JSON serialization for filesystem storage

## Current Status

- **Lines of Rust**: ~15,000+ across 30+ source files
- **Crates**: 5 (core, agents, cli, server, storage)
- **Agents**: 7 implemented (Planner, Scheduler, NixExecutor, FlakeAnalyzer, AICodeReviewer, StorageManager, Builder)
- **Task Types**: 15 defined
- **Message Types**: 50+ defined
- **HTTP Endpoints**: 12 REST + 3 webhook handlers

---

# Phase 1: Distributed Message Bus (Priority: HIGH)

## Goal: Replace in-memory channels with NATS for distributed execution

## Tasks

### 1.1 Add NATS Dependency
```toml
[dependencies]
nats = { version = "0.25", features = ["jetstream"] }
async-nats = "0.32"
```

### 1.2 Create NATS Message Bus Adapter
- File: `agentflow-core/src/bus.rs`
- Trait: `MessageBus` with NATS and in-memory implementations
- Support for pub/sub and request/reply patterns

### 1.3 Update Agent Trait
```rust
#[async_trait]
pub trait Agent {
    async fn handle_message(&mut self, message: AgentMessage, ctx: &AgentContext) -> Result<>();
    
    // New: Get agent's NATS subject prefix
    fn subject(&self) -> String {
        format!("agentflow.{}.{}", self.agent_type(), self.name())
    }
}
```

### 1.4 Configuration
- Environment variables for NATS URL, credentials
- Fallback to in-memory for testing

### 1.5 Deliverables
- [ ] `agentflow-core/src/bus.rs` with MessageBus trait
- [ ] NATS implementation in `agentflow-server`
- [ ] Agents updated to use MessageBus
- [ ] Configuration system

### 1.6 Estimate
- **Time**: 1-2 weeks
- **Complexity**: Medium
- **Risk**: Low (NATS is well-documented, Rust crates are mature)

---

# Phase 2: HTTP Gateway (Priority: HIGH)

## Goal: REST/gRPC API for external clients

## Tasks

### 2.1 HTTP Server Architecture
```
┌─────────────────┐
│   HTTP Server   │  ← axum/hyper
├─────────────────┤
│   API Layer     │  ← RESTJSON or gRPC
├─────────────────┤
│   Message Bus   │  ← NATS or in-memory
├─────────────────┤
│   Agents        │  ← Planner, Scheduler, etc.
└─────────────────┘
```

### 2.2 REST API Endpoints

#### Task Management
```
POST   /api/v1/tasks              - Submit new task
GET    /api/v1/tasks              - List tasks (with filters)
GET    /api/v1/tasks/:id          - Get task details
PATCH  /api/v1/tasks/:id          - Update task (priority, status)
DELETE /api/v1/tasks/:id          - Cancel task
```

#### Agent Management
```
GET    /api/v1/agents             - List all agents
GET    /api/v1/agents/:id         - Get agent details
POST   /api/v1/agents/register    - Register new agent
DELETE /api/v1/agents/:id/deregister - Deregister agent
```

#### Flake Operations
```
POST   /api/v1/flakes/analyze     - Analyze a flake
POST   /api/v1/flakes/evaluate    - Evaluate a flake
GET    /api/v1/flakes/:url/outputs - Get flake outputs
```

#### System
```
GET    /api/v1/health             - Health check
GET    /api/v1/metrics            - Prometheus metrics
GET    /api/v1/status             - System status
```

### 2.3 Implementation

#### 2.3.1 Add axum to Cargo.toml
```toml
[dependencies]
axum = "0.7"
tokio = { version = "1.0", features = ["full"] }
tower = "0.4"
tower-http = { version = "0.5", features = ["cors", "trace"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
```

#### 2.3.2 Server Structure
```
agentflow-server/
├── src/
│   ├── lib.rs           # Library exports
│   ├── main.rs          # Binary entry point
│   ├── api/
│   │   ├── mod.rs       # API module
│   │   ├── tasks.rs     # Task endpoints
│   │   ├── agents.rs    # Agent endpoints
│   │   ├── flakes.rs    # Flake endpoints
│   │   └── system.rs    # System endpoints
│   ├── state.rs         # Server state
│   └── config.rs        # Configuration
└── Cargo.toml
```

#### 2.3.3 State Management
```rust
#[derive(Clone)]
pub struct ServerState {
    pub task_store: Arc<dyn TaskStore + Send + Sync>,
    pub agent_store: Arc<dyn StateStore + Send + Sync>,
    pub message_bus: Arc<dyn MessageBus + Send + Sync>,
    pub config: ServerConfig,
}
```

### 2.4 Deliverables
- [ ] axum HTTP server in `agentflow-server`
- [ ] REST API endpoints for tasks, agents, flakes
- [ ] OpenAPI/Swagger documentation
- [ ] Health check and metrics endpoints

### 2.5 Estimate
- **Time**: 2-3 weeks
- **Complexity**: Medium
- **Risk**: Low

---

# Phase 3: Additional Agent Types (Priority: HIGH)

## Goal: Expand agent capabilities for production use

## Status

- [x] AICodeReviewerAgent (implemented ~750 lines)
- [x] StorageManagerAgent (implemented ~800 lines, multi-backend support)
- [x] BuilderAgent (implemented, multi-arch Nix builds)
- [ ] GitSyncAgent
- [ ] MoeSyncAgent
- [ ] MoeVerifyAgent
- [ ] MoeGCAgent
- [ ] QEMUTestAgent
- [ ] GitHubStatusAgent
- [ ] MatrixNotifierAgent

## New Agents to Implement

### 3.1 AICodeReviewer Agent
**Purpose**: AI-powered code review for Nix flakes

**Capabilities**:
- Analyze changes in PRs
- Provide feedback on Nix code quality
- Detect anti-patterns
- Suggest improvements

**Configuration**:
```rust
pub struct AICodeReviewerAgent {
    llm_endpoint: String,
    model: String,
    temperature: f32,
    max_tokens: u32,
    prompts: CodeReviewPrompts,
}
```

**Inputs**:
- Git diff
- Flake URL
- PR description

**Outputs**:
- Code review comments
- Quality score
- Suggested fixes

### 3.2 StorageManager Agent
**Purpose**: Manage artifact storage and caching

**Capabilities**:
- Store build artifacts
- Cache Nix store paths
- Garbage collection
- Sync with Mœ storage
- Serve cached artifacts

**Configuration**:
```rust
pub struct StorageManagerAgent {
    storage_backend: StorageBackend,  // Local, S3, Mœ, etc.
    cache_path: PathBuf,
    retention_policy: RetentionPolicy,
    upload_bandwidth_limit: Option<u64>,
}
```

**Storage Backends**:
- Local filesystem
- S3-compatible (MinIO)
- Mœ storage (integration with existing opendesk)
- HTTP cache

### 3.3 Builder Agent (Enhanced NixExecutor)
**Purpose**: General-purpose build execution

**Capabilities**:
- Parallel builds
- Resource limits (CPU, memory)
- Build isolation (containers, VMs)
- Build result caching
- Retry logic

### 3.4 GitSync Agent
**Purpose**: Monitor and sync Git repositories

**Capabilities**:
- Webhook handling (GitHub, GitLab, Forgejo)
- Polling for changes
- Branch/tag filtering
- Commit status reporting

### 3.5 CacheAgent
**Purpose**: Manage shared build cache

**Capabilities**:
- Distributed caching
- Cache invalidation
- Cache warming
- Statistics

### 3.6 NotificationAgent
**Purpose**: Send notifications for events

**Capabilities**:
- Matrix messages
- Email
- Webhooks
- Discord/Slack (optional)

**Configuration**:
```rust
pub struct NotificationAgent {
    matrix_homeserver: String,
    matrix_user: String,
    matrix_password: String,
    email_smtp: Option<SmtpConfig>,
    webhooks: Vec<WebhookConfig>,
}
```

## Agent Implementation Priority

| Agent | Priority | Complexity | Estimate | Dependencies |
|-------|----------|------------|----------|--------------|
| StorageManager | HIGH | Medium | 3-5 days | Mœ integration |
| GitSync | HIGH | Medium | 3-5 days | Webhook handling |
| NotificationAgent | MEDIUM | Low | 2-3 days | Matrix SDK |
| AICodeReviewer | MEDIUM | High | 1-2 weeks | LLM integration |
| CacheAgent | LOW | Medium | 3-5 days | StorageManager |
| Builder (enhanced) | LOW | Medium | 3-5 days | NixExecutor |

### 3.7 Deliverables
- [ ] StorageManager agent
- [ ] GitSync agent
- [ ] NotificationAgent
- [ ] AICodeReviewer agent (if LLM access available)

### 3.8 Estimate
- **Time**: 3-4 weeks
- **Complexity**: Medium-High
- **Risk**: Medium (LLM integration uncertainty)

---

# Phase 4: opendesk Integration (Priority: HIGH)

## Goal: Full integration with opendesk infrastructure

## Tasks

### 4.1 Update Helm Charts

#### argunix Helm Chart Updates
```yaml
# values.yaml additions
agentflow:
  enabled: true
  image: ghcr.io/tobias-weiss-ai-xr/agentflow:latest
  replicaCount: 1
  
  server:
    port: 8080
    replicas: 1
    resources:
      requests:
        cpu: 500m
        memory: 512Mi
      limits:
        cpu: 2000m
        memory: 2Gi
    
  agents:
    planner: { enabled: true, replicas: 1 }
    scheduler: { enabled: true, replicas: 1 }
    nixExecutor: { enabled: true, replicas: 2 }
    flakeAnalyzer: { enabled: true, replicas: 1 }
    storageManager: { enabled: true, replicas: 1 }
    gitSync: { enabled: true, replicas: 1 }
    notification: { enabled: true, replicas: 1 }
    
  nats:
    enabled: true
    auth: { enabled: true }
```

#### New Helm Chart: agentflow
Create separate chart or extend argunix chart with AgentFlow components.

### 4.2 Configuration Management

**ConfigMap for AgentFlow**:
```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: agentflow-config
data:
  nats-url: "nats://agentflow-nats:4222"
  storage-backend: "s3"
  s3-endpoint: "http://minio:9000"
  matrix-homeserver: "https://matrix.org"
```

**Secrets**:
```yaml
apiVersion: v1
kind: Secret
metadata:
  name: agentflow-secrets
stringData:
  nats-password: "..."
  matrix-password: "..."
  s3-access-key: "..."
  s3-secret-key: "..."
```

### 4.3 Service Discovery

- Agents register with Kubernetes service discovery
- NATS used for internal communication
- External API via Ingress

### 4.4 Ingress Configuration
```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: agentflow-ingress
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt-prod
spec:
  tls:
  - hosts: [agentflow.opendesk.edu]
    secretName: agentflow-tls
  rules:
  - host: agentflow.opendesk.edu
    http:
      paths:
      - path: /api/v1
        pathType: Prefix
        backend:
          service:
            name: agentflow-server
            port:
              number: 8080
```

### 4.5 Monitoring

**ServiceMonitor for Prometheus**:
```yaml
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: agentflow-monitor
spec:
  selector:
    matchLabels:
      app: agentflow-server
  endpoints:
  - port: http
    interval: 30s
    path: /metrics
```

### 4.6 Deliverables
- [ ] Updated argunix Helm chart with AgentFlow
- [ ] Separate agentflow Helm chart (optional)
- [ ] Kubernetes manifests for NATS
- [ ] ConfigMaps and Secrets
- [ ] Ingress configuration
- [ ] ServiceMonitor for Prometheus

### 4.7 Estimate
- **Time**: 2-3 weeks
- **Complexity**: Medium
- **Risk**: Low (opendesk patterns already established)

---

# Phase 5: Advanced Features (Priority: Low)

## Tasks for Future Consideration

### 5.1 gRPC API
- Add gRPC alongside REST
- Better for internal service-to-service communication
- Protobuf definitions in `/proto`

### 5.2 Redis Integration
- Alternative to NATS for simpler deployments
- Redis Streams for message queuing
- Redis for caching

### 5.3 Agent Auto-scaling
- KEDA for event-driven scaling
- Scale agents based on queue depth
- Scale NixExecutors based on build demand

### 5.4 Distributed Tracing
- OpenTelemetry integration
- Jaeger for visualization
- Trace requests through agent chain

### 5.5 Rate Limiting
- Protect against abuse
- Per-user/per-repo rate limits
- Configurable limits

### 5.6 Authentication & Authorization
- JWT token validation
- OAuth2 integration (GitHub, GitLab)
- Role-based access control

---

# Implementation Order Recommendation

Based on dependencies and value:

```
Phase 1: NATS Message Bus    (1-2 weeks)  ⬅️  HIGH PRIORITY
     ↓
Phase 4: opendesk Integration (2-3 weeks) ⬅️  HIGH PRIORITY
     ↓
Phase 2: HTTP Server         (2-3 weeks)  ⬅️  HIGH PRIORITY
     ↓
Phase 3: More Agents         (3-4 weeks)  ⬅️  MEDIUM PRIORITY
     ↓
Phase 5: Advanced Features   (Ongoing)
```

**Total for Phases 1-4: 8-11 weeks**

---

# Resource Requirements

## Development
- 1-2 Rust developers
- Access to opendesk Kubernetes cluster
- NATS server (test instance)
- LLM access (for AICodeReviewer, optional)

## Production
- Kubernetes cluster (existing opendesk)
- NATS server (3-node cluster for HA)
- ~2-4 CPU cores for AgentFlow components
- ~4-8 GB RAM
- Storage: 10-50 GB for artifacts (grows over time)

---

# Success Criteria

## Phase 1 (NATS)
- [ ] Agents can communicate via NATS
- [ ] In-memory fallback still works
- [ ] Message ordering preserved
- [ ] Error handling works

## Phase 2 (HTTP)
- [ ] REST API works with curl
- [ ] OpenAPI docs available
- [ ] Health checks pass
- [ ] Metrics exported to Prometheus

## Phase 3 (Agents)
- [ ] StorageManager persistent storage
- [ ] GitSync handles webhooks
- [ ] NotificationAgent sends Matrix messages

## Phase 4 (opendesk)
- [ ] Deployed to staging
- [ ] All Helm charts work
- [ ] Integration tests pass
- [ ] Monitoring configured

---

# Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| NATS performance | Low | Medium | Benchmark before production |
| Rust async complexity | Medium | High | Use established patterns |
| opendesk cluster capacity | Low | Medium | Monitor resource usage |
| LLM costs | Medium | Medium | Rate limit, cache responses |
| Security vulnerabilities | Medium | High | Regular dependency updates |

---

# Dependencies on Other Projects

1. **Mœ Storage**: StorageManager needs Mœ API
2. **Matrix**: NotificationAgent needs Matrix homeserver access
3. ** elaboratedätt**: LLM access for AICodeReviewer
4. **opendesk infra**: Kubernetes cluster, Helm, monitoring stack

---

# Next Immediate Actions

1. **Start Phase 1: NATS Integration**
   - Add NATS dependencies to agentflow-core
   - Create MessageBus trait
   - Implement NATS message bus
   - Test with 2+ agents

2. **Prepare Phase 4: Helm Chart Updates**
   - Update argunix Helm chart
   - Add agentflow components
   - Test locally with kind/minikube

3. **Start Phase 2: HTTP Server**
   - Add axum to agentflow-server
   - Implement basic health endpoint
   - Add task submission endpoint

---

# Contacts

- **Project Lead**: Tobias Weiss
- **Repository**: https://github.com/tobias-weiss-ai-xr/argunix
- **Issues**: GitHub Issues
- **Discussion**: Matrix room (TBD)
