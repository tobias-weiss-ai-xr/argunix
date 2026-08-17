# AgentFlow Development Task Queue

This document contains the development tasks for completing the AgentFlow system.

## Task Descriptions

Each task below should be implemented as an independent agent module.

---

### 1. BuilderAgent

**Priority**: High  
**Estimated Effort**: 3-4 hours  
**Dependencies**: StorageManagerAgent

#### Purpose
Enhanced Nix build agent that handles multi-architecture builds, cross-compilation, and integration with Nix's remote build features.

#### Responsibilities
- Execute `nix build` commands
- Handle multiple architectures (x86_64-linux, aarch64-linux, etc.)
- Support cross-compilation
- Upload build artifacts to StorageManager
- Cache build results
- Handle build dependencies
- Parse and validate Nix expressions

#### Messages to Handle
- `ExecuteBuild` - Build a derivation
- `BuildComplete` - Build finished
- `BuildFailed` - Build error
- `RequestBuild` - Request a build

#### Configuration
```json
{
  "nix_path": ["/nix/store"],
  "supported_systems": ["x86_64-linux", "aarch64-linux"],
  "max_concurrent_builds": 4,
  "cache_builds": true,
  "use_remote": false,
  "remote_builder": null
}
```

#### Capabilities
- nix-build
- cross-compilation
- multi-arch
- artifact-upload
- cache-management

---

### 2. GitSyncAgent

**Priority**: High  
**Estimated Effort**: 3 hours  
**Dependencies**: None

#### Purpose
Synchronize Git repositories and handle webhook notifications from GitHub, GitLab, and Forgejo.

#### Responsibilities
- Poll repositories for changes
- Handle push/pull request webhooks
- Clone/update repositories
- Detect flake.nix changes
- Trigger downstream builds
- Manage repository cache

#### Messages to Handle
- `SyncRepository` - Sync a repository
- `RepositorySynced` - Sync complete
- `WebhookReceived` - Incoming webhook
- `PollRepository` - Poll for changes

#### Configuration
```json
{
  "repositories": {
    "argunix": {
      "url": "https://github.com/tobias-weiss-ai-xr/argunix",
      "branch": "main",
      "poll_interval": 60,
      "webhook_secret": null,
      "flake_path": "."
    }
  },
  "providers": {
    "github": { "enabled": true },
    "gitlab": { "enabled": false },
    "forgejo": { "enabled": true }
  },
  "cache_path": "/var/cache/agentflow/repos",
  "max_cache_size": "10G"
}
```

#### Capabilities
- git-sync
- webhook-handler
- repository-polling
- flake-detection

---

### 3. MoeSyncAgent

**Priority**: Medium  
**Estimated Effort**: 2-3 hours  
**Dependencies**: StorageManagerAgent

#### Purpose
Synchronize data with Mœ self-sovereign storage, manage identities and generations.

#### Responsibilities
- Sync local objects to Mœ
- Verify Mœ object integrity
- Manage identity keys
- Handle generation transitions
- Synchronize with other Mœ peers
- Backup and restore Mœ data

#### Messages to Handle
- `SyncToMoe` - Sync data to Mœ
- `SyncFromMoe` - Pull data from Mœ
- `VerifyMoeObject` - Verify object integrity
- `MoeSyncComplete` - Sync finished

#### Configuration
```json
{
  "identity": {
    "name": "argunix-ci",
    "fingerprint": "sha256-...",
    "private_key": null
  },
  "peers": [
    { "url": "https://moe.opendesk.works", "trusted": true }
  ],
  "namespace": "ci-artifacts",
  "sync_interval": 300,
  "auto_verify": true
}
```

#### Capabilities
- moe-sync
- identity-management
- generation-management
- peer-synchronization

---

### 4. MoeVerifyAgent

**Priority**: Medium  
**Estimated Effort**: 2 hours  
**Dependencies**: MoeSyncAgent

#### Purpose
Verify Mœ content-addressable storage integrity and authenticity.

#### Responsibilities
- Verify object hashes
- Check cryptographic signatures
- Validate content against claims
- Detect tampering or corruption
- Generate verification reports
- Audit storage integrity

#### Messages to Handle
- `VerifyObject` - Verify a specific object
- `VerifyGeneration` - Verify all objects in a generation
- `VerificationResult` - Result of verification
- `AuditStorage` - Audit all storage

#### Configuration
```json
{
  "strict_mode": true,
  "check_signatures": true,
  "audit_interval": 86400, // 24 hours
  "report_path": "/var/log/agentflow/verification-reports"
}
```

#### Capabilities
- moe-verification
- integrity-checking
- signature-validation
- audit-reporting

---

### 5. MoeGCAgent

**Priority**: Medium  
**Estimated Effort**: 2 hours  
**Dependencies**: MoeSyncAgent

#### Purpose
Garbage collect old Mœ objects and manage storage lifecycle.

#### Responsibilities
- Identify old/unused objects
- Remove expired generations
- Compact storage
- Clean up orphaned references
- Enforce retention policies
- Generate cleanup reports

#### Messages to Handle
- `GarbageCollect` - Start GC
- `GCCycleComplete` - GC finished
- `CleanupOldObjects` - Remove old objects
- `CompactStorage` - Compact storage

#### Configuration
```json
{
  "retention_policies": {
    "build-artifacts": { "keep_days": 30 },
    "source-code": { "keep_days": 365 },
    "releases": { "keep_days": 2555 } // ~7 years
  },
  "gc_interval": 259200, // 3 days
  "dry_run": false,
  "max_deleted_per_run": 1000
}
```

#### Capabilities
- moe-gc
- garbage-collection
- storage-compaction
- retention-management

---

### 6. QEMUTestAgent

**Priority**: Medium  
**Estimated Effort**: 4 hours  
**Dependencies**: StorageManagerAgent

#### Purpose
Run tests in QEMU virtual machines for cross-platform compatibility testing.

#### Responsibilities
- Provision QEMU VMs
- RunTests in isolated environments
- Support multiple architectures
- Capture test output and logs
- Generate test reports
- Clean up VMs after tests
- Cache VM images

#### Messages to Handle
- `RunTests` - Execute tests
- `TestComplete` - Tests finished
- `TestFailed` - Tests failed
- `ProvisionVM` - Create a VM
- `DestroyVM` - Remove a VM

#### Configuration
```json
{
  "qemu_path": "/usr/bin/qemu-system-x86_64",
  "images": {
    "nixos": {
      "url": "https://.../nixos-qemu.qcow2",
      "arch": "x86_64"
    }
  },
  "network": {
    "mode": "user",
    "forward_ports": [22, 80, 443]
  },
  "timeouts": {
    "provision": 300,
    "test": 600,
    "cleanup": 60
  },
  "cache_images": true
}
```

#### Capabilities
- qemu-testing
- cross-platform
- vm-management
- test-execution
- log-capture

---

### 7. GitHubStatusAgent

**Priority**: Medium  
**Estimated Effort**: 2-3 hours  
**Dependencies**: GitSyncAgent

#### Purpose
Post build status to GitHub commit status API.

#### Responsibilities
- Create GitHub status checks
- Update status (pending, success, failure)
- Post detailed build logs
- Link to build artifacts
- Handle GitHub API rate limiting
- Manage repository permissions

#### Messages to Handle
- `PostStatus` - Post a status
- `StatusPosted` - Status posted successfully
- `UpdateStatus` - Update existing status
- `NotifyGitHub` - Send notification

#### Configuration
```json
{
  "github_token": null, // Set via environment
  "default_context": "argunix-ci",
  "base_url": "https://api.github.com",
  "rate_limit": {
    "requests_per_minute": 60,
    "retry_on_limit": true,
    "retry_delay": 10
  },
  "link_artifacts": true
}
```

#### Environment Variables
- `GITHUB_TOKEN` - GitHub personal access token
- `GITHUB_APP_ID` - GitHub App ID (optional)
- `GITHUB_APP_INSTALLATION_ID` - GitHub App installation ID (optional)
- `GITHUB_APP_PRIVATE_KEY` - GitHub App private key (optional)

#### Capabilities
- github-status
- commit-status
- pull-request-status
- rate-limit-management

---

### 8. MatrixNotifierAgent

**Priority**: Medium  
**Estimated Effort**: 2-3 hours  
**Dependencies**: None

#### Purpose
Send notifications via Matrix protocol to rooms and users.

#### Responsibilities
- Connect to Matrix server
- Send messages to rooms
- Format notification messages
- Handle HTML and markdown formatting
- Manage message history
- Send file attachments

#### Messages to Handle
- `SendNotification` - Send a notification
- `NotificationSent` - Notification sent successfully
- `BroadcastMessage` - Send to multiple rooms
- `SendFile` - Send a file attachment

#### Configuration
```json
{
  "homeserver": "https://matrix.org",
  "username": "argunix-bot",
  "password": null, // Set via environment
  "device_id": null,
  "rooms": {
    "alerts": "!alerts:opendesk.works",
    "builds": "!builds:opendesk.works",
    "general": "!general:opendesk.works"
  },
  "message_formats": {
    "build_started": "Build started: {repo}@{ref}",
    "build_complete": "Build succeeded: {repo}@{ref} ({duration})",
    "build_failed": "Build failed: {repo}@{ref}\nError: {error}"
  },
  "html_enabled": true,
  "markdown_enabled": true
}
```

#### Environment Variables
- `MATRIX_PASSWORD` - Matrix password
- `MATRIX_ACCESS_TOKEN` - Matrix access token (alternative)

#### Capabilities
- matrix-notify
- room-messaging
- file-attachments
- message-formatting

---

## Implementation Checklist

- [ ] BuilderAgent
- [ ] GitSyncAgent
- [ ] MoeSyncAgent
- [ ] MoeVerifyAgent
- [ ] MoeGCAgent
- [ ] QEMUTestAgent
- [ ] GitHubStatusAgent
- [ ] MatrixNotifierAgent

---

## Implementation Template

Each agent should follow this structure:

```rust
// src/{agent_name}/mod.rs

use agentflow_core::{
    Agent, AgentContext, AgentDefinition, AgentStatus, AgentType, AgentMessage, Result,
};
use tokio::sync::mpsc;
use std::collections::HashSet;
use std::sync::Arc;

/// Agent configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct {AgentName}Config { /* fields */ }

/// Agent type definitions
pub type {AgentName}Error = /* error type */;

/// Main agent struct
pub struct {AgentName}Agent {
    definition: AgentDefinition,
    sender: mpsc::Sender<AgentMessage>,
    task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    config: {AgentName}Config,
    // Add agent-specific state
}

impl {AgentName}Agent {
    pub fn new(sender: mpsc::Sender<AgentMessage>, task_store: Arc<...>, config: {AgentName}Config) -> Self { /* */ }
    
    pub fn from_definition(definition: &AgentDefinition, sender: mpsc::Sender<AgentMessage>, task_store: Arc<...>) -> Result<Self> { /* */ }
    
    // Add agent-specific methods
}

#[async_trait::async_trait]
impl Agent for {AgentName}Agent {
    fn name(&self) -> &str { &self.definition.name }
    
    fn agent_type(&self) -> AgentType { self.definition.agent_type.clone() }
    
    fn capabilities(&self) -> &HashSet<String> { &self.definition.capabilities }
    
    async fn handle_message(&mut self, message: AgentMessage, _ctx: &AgentContext) -> Result<()> {
        match message {
            // Handle agent-specific messages
            _ => { /* handle or ignore */ }
        }
        Ok(())
    }
    
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> { /* */ Ok(()) }
    
    async fn on_shutdown(&mut self) -> Result<()> { /* */ Ok(()) }
    
    fn status(&self) -> AgentStatus { self.definition.status.clone() }
}

#[cfg(test)]
mod tests { /* tests */ }
```

---

## File Structure

```
agentflow/
├── Cargo.toml
├── agentflow-agents/
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs
│   │   ├── ai_code_reviewer/
│   │   │   └── mod.rs
│   │   ├── flake_analyzer/
│   │   │   └── mod.rs
│   │   ├── nix_executor/
│   │   │   └── mod.rs
│   │   ├── planner/
│   │   │   └── mod.rs
│   │   ├── scheduler/
│   │   │   └── mod.rs
│   │   ├── storage_manager/
│   │   │   └── mod.rs
│   │   ├── builder/          # <-- NEW
│   │   │   └── mod.rs
│   │   ├── git_sync/         # <-- NEW
│   │   │   └── mod.rs
│   │   ├── moe_sync/         # <-- NEW
│   │   │   └── mod.rs
│   │   ├── moe_verify/       # <-- NEW
│   │   │   └── mod.rs
│   │   ├── moe_gc/           # <-- NEW
│   │   │   └── mod.rs
│   │   ├── qemu_test/        # <-- NEW
│   │   │   └── mod.rs
│   │   ├── github_status/    # <-- NEW
│   │   │   └── mod.rs
│   │   └── matrix_notifier/  # <-- NEW
│   │       └── mod.rs
│   └── tests/
```

---

## Testing Strategy

Each agent should include:
1. Unit tests for core functionality
2. Integration tests with mock message bus
3. Message handling tests
4. Error handling tests

Use `cargo test` to run all tests.

---

## Documentation Requirements

Each agent must have:
1. Module-level documentation (`//!` comments)
2. Type documentation for structs, enums, traits
3. Method documentation for public APIs
4. Example usage in doc comments where applicable
5. README section in agentflow/README.md

---

## Merge Strategy

1. Each agent can be developed independently
2. Create separate branches for each agent
3. Merge when all tests pass
4. Update documentation after each merge
5. Tag releases after major milestones
