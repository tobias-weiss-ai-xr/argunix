# AgentFlow / TaskFleet

<!--
SPDX-FileCopyrightText: 2026 AgentFlow Contributors
SPDX-License-Identifier: Apache-2.0
-->

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![status](https://github.com/tobias-weiss-ai-xr/argunix/actions/workflows/ci.yaml/badge.svg)](https://github.com/tobias-Weiss-ai-xr/argunix/actions)

**AgentFlow / TaskFleet** is a sovereign, agent-driven CI/CD platform that unifies:

- 🥝 **argunix's** Nix-native CI concepts
- 👑 **Mœ Sovereignty** self-sovereign computing principles
- 🤖 **Intelligent agents** for orchestration

## 🌟 Features

### From argunix
- ✅ Nix flake-first approach
- ✅ Declarative configuration
- ✅ Reproducible builds
- ✅ Safe third-party PR handling
- ✅ Efficient DAG-based scheduling

### From Mœ Sovereignty
- ✅ Self-hosted deployment
- ✅ Multi-generational storage
- ✅ Sovereign identity (ed25519)
- ✅ Zero-trust security
- ✅ Resilient to network partitions

### AgentFlow/TaskFleet
- ✅ Agent-based architecture
- ✅ Task-driven workflows
- ✅ Dynamic orchestration
- ✅ Full observability
- ✅ Knowledge graph

## 📖 Documentation

| Document | Description |
|----------|-------------|
| [AGENTFLOW-SUMMARY.md](../AGENTFLOW-SUMMARY.md) | High-level overview |
| [AGENTFLOW-MOE-DESIGN.md](../AGENTFLOW-MOE-DESIGN.md) | Full architecture design |
| [AGENTFLOW-ROADMAP.md](../AGENTFLOW-ROADMAP.md) | Implementation roadmap |
| [AGENTFLOW-QUICKSTART.md](../AGENTFLOW-QUICKSTART.md) | Get started in 5 minutes ✨ |

## 🚀 Quick Start

```bash
# Clone the repository
git clone https://github.com/tobias-weiss-ai-xr/argunix
cd argunix/agentflow

# Build core library
cd agentflow-core
cargo build --release

# Run tests
cargo test
```

See [AGENTFLOW-QUICKSTART.md](../AGENTFLOW-QUICKSTART.md) for detailed instructions.

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    AGENTFLOW ARCHITECTURE                     │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  ┌────────────────────────── CONTROL PLANE ──────────────────┐ │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌──────────┐     │ │
│  │  │ Planner │  │Schedulr │  │ Orchest │  │ Monitor  │     │ │
│  │  │  Agent  │──►│ Agent   │──►│ rator   │──►│  Agent   │     │ │
│  │  └─────────┘  └─────────┘  └─────────┘  └──────────┘     │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                    │                            │
│  ┌────────────────────── TASK QUEUE & STATE ──────────────────┐ │
│  │  ┌─────────────────┐  ┌─────────────────┐                   │ │
│  │  │   Task Queue    │  │   Knowledge     │                   │ │
│  │  │  (Prioritized)  │──►│   Graph        │                   │ │
│  │  └─────────────────┘  └─────────────────┘                   │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                    │                            │
│  ┌────────────────────── EXECUTION PLANE ──────────────────────┐ │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐        │ │
│  │  │argunix  │  │ Mœ Node │  │AI Agent │  │Builder  │        │ │
│  │  │ Builder │  │ Worker  │  │ Runner  │  │ x86_64  │        │ │
│  │  └─────────┘  └─────────┘  └─────────┘  └─────────┘        │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                    │                            │
│  ┌──────────────────── MŒ STORAGE ────────────────────────────┐ │
│  │  Gen0────►Gen1────►Gen2────►...────►GenN                    │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

## 📦 Crates

| Crate | Description | Status |
|-------|-------------|--------|
| `agentflow-core` | Core types and traits | ✅ Starting implementation |
| `agentflow-agents` | Agent implementations | 📋 Designed |
| `agentflow-cli` | Command-line interface | 📋 Designed |
| `agentflow-server` | HTTP/gRPC API server | 📋 Designed |
| `agentflow-storage` | Storage backends | 📋 Designed |

## 🛠️ Usage

### As a Library
```toml
# Cargo.toml
[dependencies]
agentflow-core = "0.1"
```

```rust
use agentflow_core::{TaskDefinition, TaskType, AgentMessage};

fn main() {
    let task = TaskDefinition::builder()
        .task_type(TaskType::NixEval)
        .flake_url(Some("github:opendesk-edu/opendesk-nix".to_string()))
        .system(Some("x86_64-linux".to_string()))
        .build()
        .unwrap();
    
    println!("Created task: {}", task.id);
}
```

### As a Service
```bash
# Run the server
cargo run --package agentflow-server -- --config config.yaml

# Submit a task
curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "task_type": "nix-eval",
    "flake_url": "github:opendesk-edu/opendesk-nix",
    "system": "x86_64-linux"
  }'
```

## 🔧 Configuration

```yaml
# config.yaml
version: "1.0"

identity:
  name: "agentflow-main"
  node_type: "control-plane"

control_plane:
  host: "0.0.0.0"
  port: 8080

storage:
  base_dir: "/var/lib/agentflow"
  backends: ["local", "s3"]
  generations: 10

agents:
  - name: "planner-1"
    type: "planner"
    capabilities: ["task-planning"]
  
  - name: "builder-x86_64-1"
    type: "builder"
    capabilities: ["nix-build", "x86_64-linux"]

sovereignty:
  trust_mode: "explicit"
  data_locality:
    - regex: "eu-.*"
      regions: ["eu-central-1"]
```

## 📊 Telemetry

AgentFlow exposes:
- **Prometheus metrics** at `/metrics`
- **OpenTelemetry traces** for distributed tracing
- **Structured logging** in JSON format
- **Health checks** at `/health`

## 🔒 Security

- **Zero-trust**: All communication requires authentication
- **Sovereign identity**: ed25519-based node identities
- **Data locality**: Enforce geographic constraints
- **Compliance tags**: Classify and protect sensitive data
- **TLS everywhere**: All endpoints use HTTPS

## 🌈 Roadmap

See [AGENTFLOW-ROADMAP.md](../AGENTFLOW-ROADMAP.md) for detailed implementation plans.

### Phases

1. **Phase 0-1 (4 weeks)**: Core system + agents
2. **Phase 2 (4 weeks)**: Mœ sovereignty features
3. **Phase 3 (4 weeks)**: AI integration
4. **Phase 4-5 (8 weeks)**: Knowledge graph + deployment
5. **Phase 6-7 (4 weeks)**: Ecosystem + polish

**Total: ~28 weeks** for full implementation

## 🤝 Contributing

1. Fork the repository
2. Create an issue for what you want to work on
3. Start coding (see [AGENTFLOW-QUICKSTART.md](../AGENTFLOW-QUICKSTART.md))
4. Submit pull requests

### Good First Issues

- Implement `PlannerAgent`
- Implement message bus
- Add CLI for submitting tasks
- Add HTTP API server
- Implement `NixExecutorAgent`
- Add Mœ storage backend
- Add identity management

## 📜 License

Licensed under **Apache License 2.0** - see [LICENSE](../LICENSE) for details.

## 📞 Contact

- **GitHub**: [tobias-weiss-ai-xr/argunix](https://github.com/tobias-weiss-ai-xr/argunix)
- **Codeberg**: [tfc/argunix](https://codeberg.org/tfc/argunix) (upstream)
- **Email**: weissto@hrz.uni-marburg.de

## 🙏 Acknowledgments

- [argunix](https://codeberg.org/tfc/argunix) - Nix-native CI inspiration
- [Mœ](https://moe.nix-community.org) - Multi-generational orchestration
- [NixOS](https://nixos.org) - Declarative operating system
- [Rust](https://rust-lang.org) - Safe, fast, and reliable

---

**AgentFlow / TaskFleet** - Sovereign Agent-Driven CI/CD for the Nix ecosystem.

*Built with ❤️ using Rust and Nix*
