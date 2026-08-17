# AgentFlow Agent Development Plan

> **Status**: Ready for dispatch via AgentFlow system  
> **Created**: 2024  
> **Version**: 1.0

## Overview

This document describes the plan for completing all remaining AgentFlow agents. Each agent can be developed independently and in parallel.

## Committed Agents ✅

| Agent | Status | Lines | Features | PR |
|-------|--------|-------|----------|----|
| PlannerAgent | ✅ Complete | ~200 | Task DAG creation, flake analysis | Merged |
| SchedulerAgent | ✅ Complete | ~300 | Task distribution, priority queue | Merged |
| NixExecutorAgent | ✅ Complete | ~250 | Nix eval/build execution | Merged |
| FlakeAnalyzerAgent | ✅ Complete | ~150 | Flake metadata analysis | Merged |
| AICodeReviewerAgent | ✅ Complete | ~750 | LLM-powered code review | Merged |
| StorageManagerAgent | ✅ Complete | ~800 | Multi-backend artifact storage | Merged |

## Pending Agents 📋

### High Priority (Next)

| # | Agent | Priority | Effort | Depends On | Status | Assigned |
|---|-------|----------|--------|------------|--------|----------|
| 1 | **BuilderAgent** | HIGH | 3-4h | StorageManager | ⏳ To Do | Available |
| 2 | **GitSyncAgent** | HIGH | 3h | None | ⏳ To Do | Available |

### Medium Priority (Mœ Integration)

| # | Agent | Priority | Effort | Depends On | Status | Assigned |
|---|-------|----------|--------|------------|--------|----------|
| 3 | **MoeSyncAgent** | MEDIUM | 2-3h | StorageManager | ⏳ To Do | Available |
| 4 | **MoeVerifyAgent** | MEDIUM | 2h | MoeSyncAgent | ⏳ To Do | Available |
| 5 | **MoeGCAgent** | MEDIUM | 2h | MoeSyncAgent | ⏳ To Do | Available |

### Medium Priority (Testing & Notifications)

| # | Agent | Priority | Effort | Depends On | Status | Assigned |
|---|-------|----------|--------|------------|--------|----------|
| 6 | **QEMUTestAgent** | MEDIUM | 4h | StorageManager | ⏳ To Do | Available |
| 7 | **GitHubStatusAgent** | MEDIUM | 2-3h | GitSyncAgent | ⏳ To Do | Available |
| 8 | **MatrixNotifierAgent** | MEDIUM | 2-3h | None | ⏳ To Do | Available |

## Dispatch Instructions

### Using AgentFlow CLI

```bash
# Submit all tasks to the PlannerAgent
cd agentflow
agentflow submit --task tasks/builder_agent.yaml
agentflow submit --task tasks/git_sync_agent.yaml
agentflow submit --task tasks/moe_agents.yaml
agentflow submit --task tasks/qemu_test_agent.yaml
agentflow submit --task tasks/notification_agents.yaml

# Or submit all at once
for task in tasks/*.yaml; do
    agentflow submit --task "$task"
done

# Check task status
agentflow tasks list
agentflow tasks status --all

# Get details on a specific task
agentflow tasks show task-builder-agent-impl-001
```

### Using HTTP API

```bash
# POST each task to the AgentFlow server
curl -X POST http://localhost:8080/api/tasks \
  -H "Content-Type: application/json" \
  -d @tasks/builder_agent.json

# Or use the  agentflow-server API
```

## Parallel Development Strategy

### Phase 1: Core Agents (Can be developed in parallel)
- **BuilderAgent** - Handles Nix builds
- **GitSyncAgent** - Handles repository sync

### Phase 2: Mœ Integration (Depends on Phase 1)
- **MoeSyncAgent** - Sync with Mœ storage
- **MoeVerifyAgent** - Verify Mœ integrity
- **MoeGCAgent** - Garbage collect Mœ

### Phase 3: Testing & Notifications (Independent)
- **QEMUTestAgent** - Cross-platform testing
- **GitHubStatusAgent** - GitHub status updates
- **MatrixNotifierAgent** - Matrix notifications

### Recommended Assignment

| Developer | Primary Tasks | Secondary Tasks |
|-----------|---------------|-----------------|
| Developer A | BuilderAgent, QEMUTestAgent | MoeGCAgent |
| Developer B | GitSyncAgent, GitHubStatusAgent | MoeSyncAgent |
| Developer C | MoeVerifyAgent, MatrixNotifierAgent | MoeGCAgent |

---

## Implementation Order Recommendations

### Option 1: Feature-Completeness (Recommended)
1. GitSyncAgent → GitHubStatusAgent (Complete Git integration)
2. BuilderAgent → QEMUTestAgent (Complete build/test pipeline)
3. MoeSyncAgent → MoeVerifyAgent → MoeGCAgent (Complete Mœ integration)
4. MatrixNotifierAgent (Standalone)

### Option 2: Dependency-Based
1. GitSyncAgent (no dependencies)
2. BuilderAgent (only StorageManager)
3. QEMUTestAgent (only StorageManager)
4. MatrixNotifierAgent (no dependencies)
5. MoeSyncAgent (+ StorageManager)
6. MoeVerifyAgent (+ MoeSyncAgent)
7. MoeGCAgent (+ MoeSyncAgent)
8. GitHubStatusAgent (+ GitSyncAgent optional)

### Option 3: Complexity-Based (Easiest First)
1. MatrixNotifierAgent (2-3h, simple HTTP client)
2. GitHubStatusAgent (2-3h, simple HTTP client)
3. GitSyncAgent (3h, git operations)
4. MoeGCAgent (1-2h, cleanup logic)
5. MoeVerifyAgent (2h, verification logic)
6. MoeSyncAgent (2-3h, sync protocol)
7. BuilderAgent (3-4h, Nix integration)
8. QEMUTestAgent (4h, VM management)

---

## Task Details

Each task has a YAML file in the `tasks/` directory with:
- Full description
- Configuration requirements
- Success criteria
- Test cases
- Implementation notes

### Task Files

```
agentflow/tasks/
├── builder_agent.yaml          # BuilderAgent
├── git_sync_agent.yaml         # GitSyncAgent
├── moe_agents.yaml             # MoeSync, MoeVerify, MoeGC (multi-task)
├── qemu_test_agent.yaml        # QEMUTestAgent
└── notification_agents.yaml    # GitHubStatus, MatrixNotifier (multi-task)
```

## Development Environment Setup

### Prerequisites

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Install required tools
sudo apt-get install -y \
    git \
    curl \
    qemu \
    nix 

# Or on NixOS
nix-env -iA \
    nixpkgs.rustc \
    nixpkgs.cargo \
    nixpkgs.git \
    nixpkgs.qemu \
    nixpkgs.nix
```

### Clone and Build

```bash
git clone git@github.com:tobias-weiss-ai-xr/argunix.git
cd argunix/agentflow
cargo build --release
```

### Test Environment

```bash
# Run all tests
cargo test --all

# Run specific agent tests
cargo test --package agentflow-agents -- builder
cargo test --package agentflow-agents -- git_sync
```

## Code Review Process

1. **Create a branch** for each agent:
   ```bash
   git checkout -b feature/builder-agent
   git checkout -b feature/git-sync-agent
   ```

2. **Implement** following the template in DEVELOPMENT_TODO.md

3. **Test thoroughly** before submitting:
   ```bash
   cargo test
   cargo clippy
   cargo fmt
   ```

4. **Submit PR** to main branch

5. **Request review** from team members

6. **Address feedback** and merge

## Quality Standards

✅ **Must Have**
- All tests passing
- Code compiles without warnings (or justified `#[allow]`)
- Proper error handling
- Documentation for all public APIs
- Module-level documentation
- Follows Rust best practices

⚠️ **Nice to Have**
- Example usage in doc comments
- Integration tests
- Benchmarks for performance-critical code
- Logging with `tracing`
- Metrics collection

❌ **Avoid**
- Unwraps in library code
- Panics in normal operation
- Blocking operations in async contexts
- Hardcoded values (use config)
- Magic numbers (use constants)

## Monitoring and Metrics

Each agent should:
- Log important events with appropriate levels (debug, info, warn, error)
- Track success/failure counts
- Measure operation durations (for performance monitoring)
- Report errors to monitoring system

### Example Logging

```rust
use tracing::{info, error, debug, warn};

info!("Starting build for {}", drv_path);
debug!("Build options: {:?}", options);
warn!("Build timed out after {}s", timeout);
error!("Build failed: {}", error);
```

## Integration Testing

After all agents are implemented:

1. **Local Testing**
   ```bash
   # Start AgentFlow server
   agentflow server
   
   # In another terminal, submit tasks
   agentflow submit --task tasks/builder_agent.yaml
   
   # Monitor progress
   agentflow tasks list --watch
   ```

2. **Docker Testing**
   ```bash
   # Build Docker image
   docker build -t agentflow .
   
   # Run in Docker
   docker run --rm -it \
     -v $(pwd)/tasks:/tasks \
     -p 8080:8080 \
     agentflow
   ```

3. **Kubernetes Testing**
   ```bash
   # Install Helm chart
   helm install agentflow ./helmfile/charts/agentflow
   
   # Check pod status
   kubectl get pods -n agentflow
   
   # View logs
   kubectl logs -f deployment/agentflow-server -n agentflow
   ```

## Task Completion Checklist

For each agent, verify:

- [ ] Code compiles without errors
- [ ] All tests pass
- [ ] No clippy warnings (or justified)
- [ ] Proper documentation
- [ ] Follows coding standards
- [ ] Handles errors gracefully
- [ ] Implements all required messages
- [ ] Works with PlannerAgent
- [ ] Works with SchedulerAgent
- [ ] Integrated into lib.rs
- [ ] Documentation updated
- [ ] Examples provided
- [ ] Configuration validated
- [ ] Performance acceptable
- [ ] Memory usage acceptable

---

## Success Criteria for All Agents

### BuilderAgent
- [ ] Can execute `nix build` commands
- [ ] Handles multiple architectures
- [ ] Caches build outputs
- [ ] Reports build status
- [ ] Handles build failures
- [ ] Supports timeouts

### GitSyncAgent
- [ ] Can clone repositories
- [ ] Can update repositories
- [ ] Detects changes
- [ ] Handles webhooks
- [ ] Supports multiple providers
- [ ] Caches repositories

### MoeSyncAgent
- [ ] Connects to Mœ server
- [ ] Stores objects in Mœ
- [ ] Loads objects from Mœ
- [ ] Manages identity
- [ ] Handles generations
- [ ] Syncs with peers

### MoeVerifyAgent
- [ ] Verifies object hashes
- [ ] Validates signatures
- [ ] Checks integrity
- [ ] Generates reports
- [ ] Audits storage
- [ ] Detects tampering

### MoeGCAgent
- [ ] Identifies old objects
- [ ] Removes expired objects
- [ ] Compacts storage
- [ ] Enforces retention
- [ ] Generates cleanup reports
- [ ] Handles errors gracefully

### QEMUTestAgent
- [ ] Provisions QEMU VMs
- [ ] Runs tests in VMs
- [ ] Captures test output
- [ ] Handles multiple architectures
- [ ] Cleans up VMs
- [ ] Reports test results

### GitHubStatusAgent
- [ ] Posts status to GitHub
- [ ] Updates existing status
- [ ] Handles rate limiting
- [ ] Formats status messages
- [ ] Links to artifacts
- [ ] Handles API errors

### MatrixNotifierAgent
- [ ] Connects to Matrix server
- [ ] Sends messages to rooms
- [ ] Formats messages (HTML/Markdown)
- [ ] Sends file attachments
- [ ] Handles authentication
- [ ] Manages sessions

---

## Timeline

### Week 1: Core Agents
- Day 1-2: BuilderAgent
- Day 3-4: GitSyncAgent

### Week 2: Mœ Integration
- Day 5-6: MoeSyncAgent
- Day 7: MoeVerifyAgent
- Day 8: MoeGCAgent

### Week 3: Testing & Notifications
- Day 9-10: QEMUTestAgent
- Day 11-12: GitHubStatusAgent + MatrixNotifierAgent

### Week 4: Testing & Polish
- Day 13-16: Integration testing, bug fixes, documentation
- Day 17-18: Performance optimization, final reviews
- Day 19-20: Release preparation

### Total: 4-5 weeks for full implementation

## Resources

### Documentation
- [AgentFlow Core Documentation](README.md)
- [Agent Development Guide](DEVELOPMENT_TODO.md)
- [OpenSpec Internal Design](AGENTFLOW-MOE-DESIGN.md)
- [Implementation Roadmap](AGENTFLOW-ROADMAP.md)

### References
- [Nix Documentation](https://nixos.org/manual/)
- [Mœ Documentation](https://moe.chemie-lernen.org/)
- [GitHub API Documentation](https://docs.github.com/en/rest)
- [Matrix Specification](https://matrix.org/docs/spec/)
- [QEMU Documentation](https://www.qemu.org/docs/)

### Contact
- Matrix: `#agentflow:opendesk.works`
- GitHub: https://github.com/tobias-weiss-ai-xr/argunix
- Email: tobias.weiss@ Pie aI xR (see GitHub for full email)

---

## Status Tracking

Use this GitHub issue for tracking:
```
https://github.com/tobias-weiss-ai-xr/argunix/issues/[NUMBER]
```

Or track in this file:

### Implementation Progress

- [ ] BuilderAgent - Ready for implementation
- [ ] GitSyncAgent - Ready for implementation
- [ ] MoeSyncAgent - Ready for implementation
- [ ] MoeVerifyAgent - Ready for implementation
- [ ] MoeGCAgent - Ready for implementation
- [ ] QEMUTestAgent - Ready for implementation
- [ ] GitHubStatusAgent - Ready for implementation
- [ ] MatrixNotifierAgent - Ready for implementation

### Current Status: All tasks ready for dispatch!

---

> ** Action Required**: Assign developers and start implementation of BuilderAgent and GitSyncAgent as highest priority.

---

*Last Updated: 2024*
*Maintainer: AgentFlow Team*
