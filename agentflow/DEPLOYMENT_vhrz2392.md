# AgentFlow Deployment on vhrz2392

## Status: ✅ PARTIALLY DEPLOYED

The AgentFlow system has been **built and partially deployed** on vhrz2392.hrz.uni-marburg.de.

---

## Deployment Details

### Machine Information
- **Hostname**: vhrz2392.hrz.uni-marburg.de
- **IP**: 172.25.3.30
- **User**: ansible
- **SSH Access**: `ssh -i ~/.ssh/id_ed25519_ssh -o KexAlgorithms=curve25519-sha256 ansible@vhrz2392.hrz.uni-marburg.de`

### Rust Environment
- **Rust**: rustc 1.97.1 (8bab26f4f 2026-07-14)
- **Cargo**: cargo 1.97.1 (c980f4866 2026-06-30)
- **Cargo Home**: `/home/ansible/.cargo`

### Repository
- **Location**: `/home/ansible/git/argunix/agentflow`
- **Branch**: main
- **Source**: https://github.com/tobias-weiss-ai-xr/argunix

---

## Deployed Components

### ✅ Built and Running

1. **agentflow-server**
   - **Status**: Running
   - **PID**: Check with `ps aux | grep agentflow-server`
   - **Port**: 3000
   - **Bind Address**: 0.0.0.0:3000
   - **Health Check**: http://vhrz2392:3000/api/v1/health
   - **API Docs**: http://vhrz2392:3000/api/v1/docs

2. **agentflow-cli**
   - **Status**: Built
   - **Location**: `/home/ansible/git/argunix/agentflow/target/release/agentflow-cli`

3. **agentflow-examples**
   - **Status**: Built
   - **dispatch_notification binary**: Built and tested
   - **Location**: `/home/ansible/git/argunix/agentflow/target/release/dispatch_notification`

4. **agentflow-core**
   - **Status**: Built
   - All 12 agents compiled

5. **agentflow-agents**
   - **Status**: Built
   - All agents compiled: PlannerAgent, SchedulerAgent, NixExecutorAgent, FlakeAnalyzerAgent, AICodeReviewerAgent, StorageManagerAgent, BuilderAgent, GitSyncAgent, QEMUTestAgent, MoeSyncAgent, MoeVerifyAgent, MoeGCAgent, GitHubStatusAgent, MatrixNotifierAgent

---

## Running Services

### HTTP Server

The AgentFlow HTTP server is running and accessible at `http://vhrz2392:3000`.

**Endpoints** (via curl):
```bash
# Health check
curl http://vhrz2392:3000/api/v1/health

# API documentation
curl http://vhrz2392:3000/api/v1/docs

# List tasks
curl http://vhrz2392:3000/api/v1/tasks

# List agents
curl http://vhrz2392:3000/api/v1/agents
```

---

## Test Results

### ✅ Successful Tests

1. **Server Startup**
   ```
   AgentFlow server starting on 0.0.0.0:3000
   API documentation: http://0.0.0.0:3000/api/v1/docs
   Health check: http://0.0.0.0:3000/api/v1/health
   ```

2. **Health Check**
   ```json
   {"status": "healthy", "version": "0.1.0", "timestamp": "2026-08-18T06:39:16Z"}
   ```

3. ** dispatch_notification Example**
   All 5 notification message types dispatched successfully:
   - ✅ PostGitHubStatus
   - ✅ UpdateGitHubStatus
   - ✅ SendMatrixNotification
   - ✅ BroadcastMatrixMessage
   - ✅ SendMatrixFile

---

## Not Yet Implemented

### ⚠️ Agent Workers
The agents are **compiled** but not yet **spawned** as worker tasks.

**Issue**: The current server implementation does not spawn agent workers. The agents handle messages but need a message bus connection.

**Workaround**: Use the `dispatch_notification` example which uses an in-memory bus, or implement agent spawning in the server.

### ⚠️ NATS Message Bus
The NATS message bus (`NatsBus`) is stubbed and not fully implemented.

**Status**: The trait exists, the struct exists, but theactual async-nats connection is not implemented.

**Impact**: Currently using InMemoryBus for testing. For distributed deployments, NATS needs to be completed.

### ⚠️ Persistent Storage
The filesystem, SQLite, and Redis storage backends are stubbed.

**Current**: Using MemoryTaskStore and MemoryAgentStore (in-memory only).

**Impact**: Task state is not persisted across restarts.

---

## File Locations on vhrz2392

```
/home/ansible/git/argunix/agentflow/
├── Cargo.toml                    # Workspace definition
├── agentflow-core/               # Core types and traits
├── agentflow-agents/             # All agent implementations
├── agentflow-cli/                # CLI binary
├── agentflow-server/             # HTTP server binary
├── agentflow-storage/            # Storage backends
├── agentflow-examples/           # Example binaries
├── target/
│   └── release/
│       ├── agentflow-server      # Built server
│       ├── agentflow-cli        # Built CLI
│       └── dispatch_notification  # Built example
└── server.pid                    # Server process ID (if running)
```

---

## Management Scripts

### Start/Stop Server

Use the management script:
```bash
# From local machine
./agentflow/scripts/run_agentflow_vhrz2392.sh start
./agentflow/scripts/run_agentflow_vhrz2392.sh stop
./agentflow/scripts/run_agentflow_vhrz2392.sh status
./agentflow/scripts/run_agentflow_vhrz2392.sh dispatch
```

### Manual Management

```bash
# SSH to vhrz2392
ssh -i ~/.ssh/id_ed25519_ssh -o KexAlgorithms=curve25519-sha256 ansible@vhrz2392.hrz.uni-marburg.de

# Navigate to project
cd /home/ansible/git/argunix/agentflow

# Start server (background)
AGENTFLOW_BIND_ADDRESS=0.0.0.0:3000 \
  /home/ansible/git/argunix/agentflow/target/release/agentflow-server &
echo $! > server.pid

# Check logs
journalctl -u agentflow-server 2>/dev/null || echo "Not a systemd service"

# Stop server
kill $(cat server.pid)
rm server.pid

# Run dispatch example
/home/ansible/git/argunix/agentflow/target/release/dispatch_notification
```

---

## Build Commands

```bash
# Full release build
cd /home/ansible/git/argunix/agentflow
/home/ansible/.cargo/bin/cargo build --workspace --release

# Check build
/home/ansible/.cargo/bin/cargo check --workspace

# Clean build
/home/ansible/.cargo/bin/cargo clean
/home/ansible/.cargo/bin/cargo build --workspace --release
```

---

## Current Limitations

1. **No Agent Workers**: The server provides HTTP API but doesn't spawn agents
2. **No NATS**: Distributed message bus not yet implemented
3. **No Persistent Storage**: Tasks stored in memory only
4. **No TLS**: HTTP only, no HTTPS
5. **No Authentication**: API is open without auth

---

## Next Steps

### Priority 1: Spawn Agents
- [ ] Modify `agentflow-server` to spawn agent workers on startup
- [ ] Connect agents to message bus
- [ ] Handle agent lifecycle (start, stop, restart)

### Priority 2: NATS Integration
- [ ] Complete `NatsBus` implementation using async-nats v0.30+
- [ ] Add JetStream support for message persistence
- [ ] Configure NATS server (via Bitnami chart or standalone)

### Priority 3: Persistent Storage
- [ ] Implement filesystem storage backend
- [ ] Complete SQLite storage backend
- [ ] Complete Redis storage backend

### Priority 4: Kubernetes Deployment
- [ ] Update Helm charts for full deployment
- [ ] Add ConfigMaps for agent configurations
- [ ] Add ServiceMonitors for Prometheus
- [ ] Deploy via ArgoCD

---

## Verification Commands

```bash
# Check server is running
curl -s http://vhrz2392:3000/api/v1/health | jq .

# Check processes
ssh -i ~/.ssh/id_ed25519_ssh -o KexAlgorithms=curve25519-sha256 ansible@vhrz2392.hrz.uni-marburg.de \
  "ps aux | grep agentflow | grep -v grep"

# Check listening ports
ssh -i ~/.ssh/id_ed25519_ssh -o KexAlgorithms=curve25519-sha256 ansible@vhrz2392.hrz.uni-marburg.de \
  "netstat -tlnp | grep 3000"

# Check disk usage
ssh -i ~/.ssh/id_ed25519_ssh -o KexAlgorithms=curve25519-sha256 ansible@vhrz2392.hrz.uni-marburg.de \
  "du -sh /home/ansible/git/argunix/agentflow/target"

# Check Rust version
ssh -i ~/.ssh/id_ed25519_ssh -o KexAlgorithms=curve25519-sha256 ansible@vhrz2392.hrz.uni-marburg.de \
  "/home/ansible/.cargo/bin/rustc --version"
```

---

## Troubleshooting

### Server Won't Start
```bash
# Check if port 3000 is in use
netstat -tlnp | grep 3000

# Kill existing process
kill $(lsof -t -i:3000) 2>/dev/null || kill -9 $(ss -tlnp | grep 3000 | awk '{print $7}' | cut -d'=' -f2 | cut -d',' -f2) 2>/dev/null
```

### Build Errors
```bash
# Update dependencies
cargo update

# Clean build
cargo clean
cargo build

# Install missing dependencies (pkg-config for OpenSSL)
sudo apt-get update
sudo apt-get install -y pkg-config libssl-dev
```

---

## Deployment Summary

| Component | Status | Port | Location |
|-----------|--------|------|----------|
| agentflow-server | ✅ Running | 3000 | `/home/ansible/git/argunix/agentflow/target/release/` |
| All Agents | ✅ Built | N/A | Compiled into binaries |
| NATS | ❌ Not configured | N/A | N/A |
| Kubernetes | ❌ Not deployed | N/A | Helm charts exist |

---

*Last updated: 2026-08-18*
*Deployed by: pi-agent*
