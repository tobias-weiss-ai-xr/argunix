# AgentFlow / TaskFleet: Implementation Roadmap

<!--
SPDX-FileCopyrightText: 2026 AgentFlow Contributors
SPDX-License-Identifier: Apache-2.0
-->

This roadmap outlines the phased implementation of **AgentFlow** - the sovereign, agent-driven CI/CD platform that unifies argunix's Nix-native concepts with Mœ Sovereignty principles.

## Phase 0: Foundation (Week 1-2)

### Objective: Establish project infrastructure and core abstractions

#### 0.1 Project Setup
- [ ] Create `agentflow` repository with proper licensing (Apache 2.0)
- [ ] Set up CI/CD pipeline using... itself (bootstrapping problem!)
- [ ] Define project structure and build system
- [ ] Set up documentation site

#### 0.2 Core Types & Interfaces (Rust)
```
agentflow-core/
├── src/
│   ├── types.rs          # AgentType, TaskType, TaskDefinition, etc.
│   ├── traits.rs         # Agent trait, Storage trait, etc.
│   ├── error.rs          # Error types
│   └── lib.rs
└── Cargo.toml
```

#### 0.3 Storage Abstraction (Mœ-inspired)
```rust
// Core storage trait for multi-generational support
pub trait StorageBackend: Send + Sync {
    async fn store(&self, data: &[u8], metadata: HashMap<String, String>) -> Result<StorageObject>;
    async fn load(&self, hash: &str) -> Result<Vec<u8>>;
    async fn list(&self, prefix: Option<&str>) -> Result<Vec<StorageObject>>;
    async fn delete(&self, hash: &str) -> Result<()>;
    async fn sync_generation(&self, from: u64, to: u64) -> Result<()>;
}

pub struct MultiGenerationalStorage {
    backends: Vec<Box<dyn StorageBackend>>,
    current_generation: u64,
    max_generations: u64,
}
```

#### 0.4 Basic Agent Framework
```rust
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    fn agent_type(&self) -> AgentType;
    fn capabilities(&self) -> &[String];
    
    async fn on_message(&self, message: AgentMessage, ctx: &AgentContext) -> Result<()>;
    async fn on_start(&mut self, ctx: &AgentContext) -> Result<()>;
    async fn on_shutdown(&mut self) -> Result<()>;
}
```

#### 0.5 Configuration System
- [ ] Implement YAML configuration parsing
- [ ] Support environment variable overrides
- [ ] Validate configuration on startup
- [ ] Support configuration hot-reload

## Phase 1: Core Agents (Week 3-6)

### Objective: Implement the essential agents for basic CI functionality

#### 1.1 Message Bus
```rust
// Using tokio channels or NATS for agent communication
pub struct MessageBus {
    publisher: mpsc::Sender<AgentMessage>,
    subscribers: HashMap<AgentType, Vec<mpsc::Receiver<AgentMessage>>>,
}

impl MessageBus {
    pub async fn publish(&self, message: AgentMessage) -> Result<()>;
    pub async fn subscribe(&mut self, agent_type: AgentType) -> mpsc::Receiver<AgentMessage>;
}
```

#### 1.2 Planner Agent
**Responsibilities:**
- Accept incoming tasks (from webhooks, CLI, API)
- Analyze Nix flakes
- Build dependency graphs
- Create task DAGs

```rust
pub struct PlannerAgent {
    bus: MessageBus,
    flake_analyzer: FlakeAnalyzer,
    nix: NixExecutor,
}

impl Agent for PlannerAgent {
    async fn on_message(&self, message: AgentMessage, ctx: &AgentContext) -> Result<()> {
        match message {
            AgentMessage::StartTask(task) => {
                // Analyze flake
                let flake_info = self.flake_analyzer.analyze(&task.flake_url, &task.flake_ref).await?;
                
                // Discover outputs
                let outputs = self.discover_outputs(&flake_info, &task.system).await?;
                
                // Build dependency graph
                let dag = self.build_dag(&outputs, &task.targets).await?;
                
                // Create tasks for each node
                let tasks = self.create_tasks(&dag).await?;
                
                // Send to scheduler
                self.bus.publish(AgentMessage::ScheduleTasks(tasks)).await?;
            }
            _ => Ok(())
        }
    }
}
```

#### 1.3 Scheduler Agent
**Responsibilities:**
- Maintain queue of pending tasks
- Match tasks to available agents/runners
- Handle task prioritization
- Manage task dependencies

```rust
pub struct SchedulerAgent {
    bus: MessageBus,
    task_queue: PriorityQueue<TaskDefinition>,
    runners: HashMap<String, RunnerInfo>,
}

impl Agent for SchedulerAgent {
    async fn on_message(&self, message: AgentMessage, ctx: &AgentContext) -> Result<()> {
        match message {
            AgentMessage::ScheduleTasks(tasks) => {
                for task in tasks {
                    self.task_queue.push(task);
                }
                self.schedule_pending().await?;
            }
            AgentMessage::RunnerAvailable(runner) => {
                self.runners.insert(runner.id.clone(), runner);
                self.schedule_pending().await?;
            }
            AgentMessage::TaskComplete(task_result) => {
                self.handle_completion(&task_result).await?;
                self.schedule_pending().await?;
            }
            _ => Ok(())
        }
    }
}
```

#### 1.4 Nix Executor Agent (argunix-inspired)
**Responsibilities:**
- Execute Nix evaluations
- Run Nix builds
- Manage Nix store
- Handle GC roots

```rust
pub struct NixExecutorAgent {
    bus: MessageBus,
    nix: NixCommand,
    store: MultiGenerationalStorage,
}

impl Agent for NixExecutorAgent {
    async fn on_message(&self, message: AgentMessage, ctx: &AgentContext) -> Result<()> {
        match message {
            AgentMessage::ExecuteNix(task) => {
                match task.task_type {
                    TaskType::NixEval => {
                        let result = self.eval_flake(&task).await?;
                        self.bus.publish(AgentMessage::TaskComplete(result)).await?;
                    }
                    TaskType::NixBuild => {
                        let result = self.build_drv(&task).await?;
                        let build = self.register_build(&result).await?;
                        self.store_build(&build).await?;
                        self.bus.publish(AgentMessage::TaskComplete(result)).await?;
                    }
                    _ => {}
                }
            }
            _ => Ok(())
        }
    }
}

impl NixExecutorAgent {
    async fn eval_flake(&self, task: &TaskDefinition) -> Result<TaskResult> {
        // Use nix-eval-jobs or nix eval
        let cmd = Command::new("nix")
            .args([
                "eval",
                &format!("--flake={}#{}", task.flake_url, task.flake_ref),
                &format!("packages.{}.*", task.system),
                "--json"
            ])
            .output()?;
        
        // Parse output and create task result
        Ok(TaskResult { ... })
    }
    
    async fn build_drv(&self, task: &TaskDefinition) -> Result<TaskResult> {
        // Build with nix build
        let drv_path = &task.drv_path;
        let cmd = Command::new("nix")
            .args(["build", drv_path, "--json"])
            .output()?;
        
        // Parse output
        Ok(TaskResult { ... })
    }
    
    async fn register_build(&self, result: &TaskResult) -> Result<Build> {
        // Create build record
        // Add GC root
        // Store in knowledge graph
        Ok(Build { ... })
    }
}
```

#### 1.5 Builder Agent (Remote Execution)
**Responsibilities:**
- Connect to remote builders
- Execute tasks on builders
- Stream logs back
- Manage builder lifecycle

#### 1.6 Storage Agent (Mœ-inspired)
**Responsibilities:**
- Manage multi-generational storage
- Handle object storage
- Sync between generations
- Manage backends (S3, IPFS, local, Nix store)

## Phase 2: Mœ Sovereignty Integration (Week 7-10)

### Objective: Add Mœ's self-sovereign computing principles

#### 2.1 Identity System
```rust
pub struct IdentityManager {
    trust_registry: TrustRegistry,
    key_store: KeyStore,
}

impl IdentityManager {
    pub fn generate_identity(&self, name: &str, node_type: &str) -> Result<SovereignIdentity> {
        // Generate ed25519 key pair
        let mut csprng = OsRng;
        let signing_key: SigningKey = SigningKey::generate(&mut csprng);
        let verifying_key = signing_key.verifying_key();
        
        let fingerprint = sha256::hash(&verifying_key.to_bytes());
        
        Ok(SovereignIdentity {
            public_key: verifying_key.to_base64_string(),
            fingerprint: format!("{:x}", fingerprint),
            name: name.to_string(),
            node_type: node_type.to_string(),
            capabilities: vec![],
            generation: 0,
            created_at: chrono::Utc::now().timestamp(),
            expires_at: None,
        })
    }
    
    pub fn verify_identity(&self, identity: &SovereignIdentity, challenge: &[u8], signature: &Signature) -> Result<()> {
        let verifying_key = VerifyingKey::from_base64_string(&identity.public_key)?;
        verifying_key.verify(challenge, signature)?;
        
        // Check in trust registry
        if !self.trust_registry.is_trusted(&identity.fingerprint) {
            return Err(anyhow::anyhow!("Identity not trusted"));
        }
        
        Ok(())
    }
}
```

#### 2.2 Plurality & Consensus
**Implementation based on Mœ concepts:**
- Each node maintains its own state
- Consensus via CRDTs (Conflict-free Replicated Data Types)
- Plurality: Multiple copies of data across nodes
- Conflict resolution via defined strategies

```rust
pub struct ConsensusManager {
    crdt: StateCRDT,
    peers: HashMap<String, PeerConnection>,
}

impl ConsensusManager {
    pub async fn sync_with_peer(&mut self, peer_id: &str) -> Result<()> {
        let peer = self.peers.get_mut(peer_id).ok_or_else(|| anyhow!("Peer not found"))?;
        
        // Exchange state using CRDT merge
        let their_state = peer.fetch_state().await?;
        let our_state = self.crdt.state();
        
        // Merge states
        let merged = our_state.merge(their_state)?;
        self.crdt.set_state(merged);
        
        // Sync back
        peer.push_state(&self.crdt.state()).await?;
        
        Ok(())
    }
}
```

#### 2.3 Multi-Generational Storage (Complete Implementation)
```rust
pub struct MoeStorage {
    base_dir: PathBuf,
    current_generation: AtomicU64,
    max_generations: u64,
    backends: Vec<Box<dyn StorageBackend>>,
    index: StorageIndex,
}

impl MoeStorage {
    /// Store object with content addressing
    pub async fn store_content(&self, data: &[u8], content_type: &str) -> Result<StorageObject> {
        let hash = self.compute_hash(data);
        
        // Check if already exists (deduplication)
        if self.index.contains(&hash) {
            return Ok(self.index.get(&hash)?);
        }
        
        // Store in current generation
        let generation = self.current_generation.load(Ordering::SeqCst);
        let obj = StorageObject {
            hash: hash.clone(),
            size: data.len() as u64,
            generation,
            content_type: content_type.to_string(),
            metadata: HashMap::new(),
            signature: None,
        };
        
        // Store in all backends
        for backend in &self.backends {
            backend.store(&hash, data, &obj.metadata).await?;
        }
        
        // Update index
        self.index.insert(obj.clone())?;
        
        // Create GC root if Nix store
        self.create_gc_root(&obj).await?;
        
        Ok(obj)
    }
    
    /// Advance to next generation
    pub async fn next_generation(&self) -> Result<()> {
        let new_gen = self.current_generation.fetch_add(1, Ordering::SeqCst) + 1;
        
        // Archive previous generation
        self.archive_generation(new_gen - 1).await?;
        
        // Cleanup old generations
        if new_gen > self.max_generations {
            let old_gen = new_gen - self.max_generations;
            self.cleanup_generation(old_gen).await?;
        }
        
        Ok(())
    }
    
    /// Archive a generation to cold storage
    pub async fn archive_generation(&self, generation: u64) -> Result<()> {
        let manifest = self.collect_generation_manifest(generation).await?;
        let manifest_data = serde_json::to_vec(&manifest)?;
        let manifest_hash = self.compute_hash(&manifest_data);
        
        // Store manifest in cold storage
        for backend in &self.backends {
            if backend.is_cold_storage() {
                backend.store(&manifest_hash, &manifest_data, &HashMap::new()).await?;
            }
        }
        
        Ok(())
    }
    
    /// Cleanup old generation from hot storage
    pub async fn cleanup_generation(&self, generation: u64) -> Result<()> {
        let objects = self.index.objects_in_generation(generation);
        
        for obj in objects {
            // Keep in cold storage but remove from hot
            for backend in &self.backends {
                if !backend.is_cold_storage() {
                    backend.delete(&obj.hash).await?;
                }
            }
        }
        
        // Remove from index
        self.index.remove_generation(generation)?;
        
        Ok(())
    }
}
```

#### 2.4 Data Locality & Compliance
```rust
pub struct SovereigntyConstraints {
    locality_rules: Vec<DataLocalityRule>,
    compliance_rules: Vec<ComplianceRule>,
}

impl SovereigntyConstraints {
    pub fn check_locality(&self, data: &StorageObject, node: &NodeInfo) -> Result<()> {
        for rule in &self.locality_rules {
            if regex::Regex::new(&rule.regex)?.is_match(&data.hash) {
                match &rule.constraint {
                    LocalityConstraint::Regions(regions) => {
                        if !regions.contains(&node.region) {
                            return Err(anyhow!("Data locality violation: node region {} not in {}", 
                                node.region, regions.join(", ")));
                        }
                    }
                    LocalityConstraint::Nodes(nodes) => {
                        if !nodes.contains(&node.id) {
                            return Err(anyhow!("Data locality violation: node {} not in {}", 
                                node.id, nodes.join(", ")));
                        }
                    }
                    LocalityConstraint::Anywhere => {}
                }
            }
        }
        Ok(())
    }
    
    pub fn check_compliance(&self, data: &StorageObject) -> ComplianceRequirement {
        for rule in &self.compliance_rules {
            if data.metadata.get("tag") == Some(&rule.tag) {
                return rule.requirement.clone();
            }
        }
        ComplianceRequirement::default()
    }
}
```

## Phase 3: AI Integration (Week 11-14)

### Objective: Add AI agents for intelligent automation

#### 3.1 AI Agent Infrastructure
```rust
pub struct AIService {
    client: LlamaCppClient,
    models: HashMap<String, ModelConfig>,
    handbooks: HashMap<String, String>,
}

impl AIService {
    pub async fn infer(&self, model: &str, prompt: &str, context: Option<&HashMap<String, String>>) -> Result<String> {
        let model_config = self.models.get(model).ok_or_else(|| anyhow!("Model not found"))?;
        
        // Build full prompt with context
        let full_prompt = self.build_prompt(model, prompt, context)?;
        
        // Call AI model
        let response = self.client.generate(&model_config.endpoint, &full_prompt, &model_config.params).await?;
        
        Ok(response)
    }
    
    pub fn build_prompt(&self, model: &str, prompt: &str, context: Option<&HashMap<String, String>>) -> Result<String> {
        // Load handbook if available
        let handbook = self.handbooks.get(model).cloned().unwrap_or_default();
        
        let mut full_prompt = String::new();
        full_prompt.push_str(&handbook);
        full_prompt.push_str("\n\n");
        
        if let Some(ctx) = context {
            full_prompt.push_str("## Context\n");
            for (key, value) in ctx {
                full_prompt.push_str(&format!("{}: {}\n", key, value));
            }
            full_prompt.push_str("\n");
        }
        
        full_prompt.push_str("## Task\n");
        full_prompt.push_str(prompt);
        
        Ok(full_prompt)
    }
}
```

#### 3.2 AI Code Review Agent
**Responsibilities:**
- Analyze code changes
- Check for security issues
- Validate against coding standards
- Provide suggestions

```rust
pub struct AICodeReviewer {
    ai: AIService,
    bus: MessageBus,
    handbook_path: PathBuf,
}

impl Agent for AICodeReviewer {
    async fn on_message(&self, message: AgentMessage, ctx: &AgentContext) -> Result<()> {
        match message {
            AgentMessage::ReviewCode(task) => {
                let handbook = std::fs::read_to_string(&self.handbook_path)?;
                
                let prompt = format!(
                    "Please review the following code changes:\n\n{}\n\n{}",
                    handbook,
                    task.code_changes
                );
                
                let context = HashMap::from([
                    ("repo".to_string(), task.repo_url),
                    ("branch".to_string(), task.branch),
                    ("pr_id".to_string(), task.pr_id.unwrap_or_default()),
                ]);
                
                let review = self.ai.infer("llama3.2:70b", &prompt, Some(&context)).await?;
                
                // Parse review (JSON or structured output)
                let structured_review = self.parse_review(&review)?;
                
                // Send back result
                self.bus.publish(AgentMessage::ReviewComplete(structured_review)).await?;
            }
            _ => Ok(())
        }
    }
}
```

#### 3.3 AI Flake Analyzer Agent
**Responsibilities:**
- Analyze Nix flakes for potential issues
- Detect dependency vulnerabilities
- Suggest optimizations
- Predict build times

#### 3.4 AI Quality Gate Agent
**Responsibilities:**
- Gate builds based on quality metrics
- Block deployments with critical issues
- Provide automated rollback recommendations

## Phase 4: Knowledge Graph & Observability (Week 15-18)

### Objective: Full observability and knowledge management

#### 4.1 Knowledge Graph Implementation
Using **Neo4j** or **TerminusDB** for semantic knowledge:

```rust
pub struct KnowledgeGraph {
    client: Neo4jClient,
}

impl KnowledgeGraph {
    pub async fn query(&self, query: &str, params: Option<HashMap<String, Value>>) -> Result<Vec<Record>> {
        self.client.execute_query(query, params).await
    }
    
    pub async fn add_build(&self, build: &Build, flake: &FlakeInfo) -> Result<()> {
        let query = r#"
            CREATE (b:Build {id: $build_id, store_path: $store_path, status: $status})
            CREATE (f:Flake {url: $flake_url, ref: $flake_ref})
            CREATE (b)-[:BUILT_FROM]->(f)
            WITH b, f
            UNWIND $dependencies AS dep
            MERGE (d:Derivation {name: dep.name, system: dep.system})
            CREATE (b)-[:DEPENDS_ON]->(d)
        "#;
        
        self.query(query, Some(serde_json::json!({
            "build_id": build.id,
            "store_path": build.store_path,
            "status": build.status.to_string(),
            "flake_url": flake.url,
            "flake_ref": flake.ref,
            "dependencies": flake.dependencies,
        }))).await?;
        
        Ok(())
    }
    
    pub async fn query_dependencies(&self, package: &str) -> Result<Vec<Dependency>> {
        let query = r#"
            MATCH (p:Package {name: $package})-[:DEPENDS_ON*1..5]->(d:Package)
            RETURN d.name AS name, d.version AS version, d.license AS license
        "#;
        
        let results = self.query(query, Some(serde_json::json!({"package": package}))).await?;
        Ok(results.into_iter().map(|r| Dependency { ... }).collect())
    }
}
```

#### 4.2 Metrics Collection
```rust
pub struct MetricsCollecter {
    prometheus: PrometheusExporter,
    open_telemetry: OpenTelemetryExporter,
}

impl MetricsCollecter {
    pub fn record_task(&self, task: &TaskDefinition, duration: f64, success: bool) {
        // Record Prometheus metrics
        prometheus::counter!("tasks_total", "total_tasks")
            .inc();
        
        prometheus::counter!("tasks_success", "successful_tasks")
            .inc_by(if success { 1.0 } else { 0.0 });
        
        prometheus::histogram!("task_duration_seconds", "task_duration")
            .observe(duration);
        
        // Record OpenTelemetry traces
        let mut span = tracer::span_builder("task_execution")
            .with_attribute(Key::new("task_id"), Value::String(task.id.clone()))
            .with_attribute(Key::new("task_type"), Value::String(task.task_type.to_string()))
            .start();
        
        span.set_attribute(Key::new("status"), Value::String(if success { "success" } else { "failure" }));
        span.end();
    }
}
```

#### 4.3 Business Intelligence
- Build success/failure rates
- Build duration trends
- Resource utilization
- Cost tracking (if using cloud build)
- Security posture
- Compliance status

## Phase 5: Deployment & Scaling (Week 19-22)

### Objective: Production-ready deployment

#### 5.1 Distribution
- [ ] Nix flake for easy installation
- [ ] Docker images (distroless)
- [ ] Kubernetes manifests
- [ ] Helm chart
- [ ] Binary releases

#### 5.2 NixOS Module
```nix
# Integrate with opendesk-nix
{
  options.services.agentflow = {
    enable = true;
    
    roles = [ "control-plane" "builder-x86_64" "ai-reviewer" ];
    
    sovereignty = {
      enable = true;
      generations = 10;
      trust_mode = "explicit";
    };
    
    storage = {
      backends = [ "local" "s3" ];
      s3 = {
        bucket = "agentflow-builds";
        region = "eu-central-1";
      };
    };
    
    ai = {
      enable = true;
      models = [ "llama3.2:70b" ];
    };
  };
}
```

#### 5.3 Kubernetes Integration
```yaml
# Helm chart for AgentFlow
apiVersion: v1
kind: Namespace
metadata:
  name: agentflow
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: agentflow-control-plane
  namespace: agentflow
spec:
  serviceName: agentflow-control-plane
  replicas: 3
  selector:
    matchLabels:
      app: agentflow
      component: control-plane
  template:
    metadata:
      labels:
        app: agentflow
        component: control-plane
    spec:
      serviceAccountName: agentflow
      containers:
      - name: control-plane
        image: ghcr.io/tobias-weiss-ai-xr/agentflow:main
        args: ["control-plane"]
        ports:
        - containerPort: 8080
          name: http
        - containerPort: 8081
          name: metrics
        volumeMounts:
        - name: storage
          mountPath: /var/lib/agentflow
        - name: config
          mountPath: /etc/agentflow
          readOnly: true
      volumes:
      - name: config
        configMap:
          name: agentflow-config
  volumeClaimTemplates:
  - metadata:
      name: storage
    spec:
      accessModes: ["ReadWriteOnce"]
      resources:
        requests:
          storage: 100Gi
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: agentflow-builder
  namespace: agentflow
spec:
  selector:
    matchLabels:
      app: agentflow
      component: builder
  template:
    metadata:
      labels:
        app: agentflow
        component: builder
    spec:
      affinity:
        nodeAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            nodeSelectorTerms:
            - matchExpressions:
              - key: kubernetes.io/arch
                operator: In
                values: ["amd64"]
      containers:
      - name: builder
        image: ghcr.io/tobias-weiss-ai-xr/agentflow:main
        args: ["builder", "--system=x86_64-linux"]
        volumeMounts:
        - name: nix-store
          mountPath: /nix/store
        - name: var-lib
          mountPath: /var/lib/agentflow
      volumes:
      - name: nix-store
        hostPath:
          path: /nix/store
      - name: var-lib
        hostPath:
          path: /var/lib/agentflow
```

#### 5.4 Scaling
- Horizontal pod autoscaling for control plane
- Dynamic builder scaling based on queue length
- Multi-region deployment support
- Cross-region synchronization

## Phase 6: Ecosystem Integration (Week 23-26)

### Objective: Integrate with external systems

#### 6.1 Forge Integration (argunix-compatible)
- [ ] GitHub
- [ ] GitLab
- [ ] Forgejo/Codeberg
- [ ] Gitea
- [ ] GitHub Enterprise Server
- [ ] GitLab Self-Hosted

#### 6.2 Storage Backends
- [ ] AWS S3
- [ ] MinIO
- [ ] Google Cloud Storage
- [ ] Azure Blob Storage
- [ ] IPFS (public and private)
- [ ] Web3.Storage
- [ ] Local filesystem
- [ ] Nix store (direct)

#### 6.3 Notification Integrations
- [ ] Slack
- [ ] Discord
- [ ] Mattermost
- [ ] Matrix
- [ ] Email (SMTP)
- [ ] Webhooks

#### 6.4 Monitoring Integrations
- [ ] Prometheus
- [ ] Grafana
- [ ] OpenTelemetry
- [ ] Jaeger
- [ ] Tempo
- [ ] Loki

## Phase 7: Polish & Documentation (Week 27-28)

### Objective: Production readiness

- [ ] Comprehensive documentation
- [ ] User guide
- [ ] Admin guide
- [ ] API documentation
- [ ] Tutorials and examples
- [ ] Troubleshooting guide
- [ ] Security audit
- [ ] Performance benchmarks
- [ ] Cost optimization guide

## Milestones Summary

| Phase | Duration | Deliverables |
|-------|----------|--------------|
| 0 | Week 1-2 | Project infrastructure, core types |
| 1 | Week 3-6 | Core agents (planner, scheduler, executor) |
| 2 | Week 7-10 | Mœ sovereignty (identity, consensus, storage) |
| 3 | Week 11-14 | AI integration (code review, analysis) |
| 4 | Week 15-18 | Knowledge graph, observability |
| 5 | Week 19-22 | Deployment, scaling |
| 6 | Week 23-26 | Ecosystem integration |
| 7 | Week 27-28 | Polish, documentation |

## Success Criteria

### MVP (After Phase 1)
- [ ] Can deploy AgentFlow
- [ ] Can submit Nix flake builds
- [ ] Builds execute successfully
- [ ] Basic task queue works

### Beta (After Phase 3)
- [ ] All core features functional
- [ ] Self-hosted deployment works
- [ ] Multi-agent coordination works
- [ ] Basic AI features available

### Production (After Phase 5)
- [ ] Full feature set
- [ ] Scalable deployment
- [ ] High availability
- [ ] Security hardened

### Ecosystem (After Phase 7)
- [ ] Rich ecosystem integrations
- [ ] Comprehensive documentation
- [ ] Active community
- [ ] Production users

## Getting Started (Right Now!)

You can start implementing Phase 0 today:

1. **Create the repository:**
   ```bash
   mkdir agentflow
   cd agentflow
   git init
   echo "# AgentFlow" > README.md
   echo "SPDX-FileCopyrightText: 2026 AgentFlow Contributors" > LICENSE
   echo "SPDX-License-Identifier: Apache-2.0" >> LICENSE
   ```

2. **Set up Rust workspace:**
   ```bash
   cargo init --name agentflow
   mkdir -p agentflow-core/src agentflow-agents/src
   ```

3. **Start with core types:**
   ```bash
   # Copy the type definitions from this document
   # into agentflow-core/src/types.rs
   ```

4. **Create first agent:**
   ```bash
   # Implement a simple echo agent in agentflow-agents/src/echo.rs
   ```

5. **Build and test:**
   ```bash
   cargo build
   cargo test
   ```

The full implementation would take approximately **6-7 months** with a small team, or **3-4 months** with a dedicated team of 3-5 developers.

Would you like me to:
1. Create the actual Phase 0 implementation (repository structure, core types)?
2. Focus on a specific component (e.g., the Nix executor agent)?
3. Create integration with your existing argunix fork?
4. Design the AI integration in more detail?
