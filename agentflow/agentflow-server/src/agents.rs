//! Agent spawning and management for AgentFlow server
//!
//! This module spawns all AgentFlow agents as background worker tasks
//! that process messages from the message bus.

use std::sync::Arc;
use std::collections::HashSet;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use agentflow_core::bus::{InMemoryBus, MessageBus};
use agentflow_core::agent::{Agent, AgentContext, AgentDefinition, AgentStatus, AgentType, TaskStore, StateStore};
use agentflow_core::state::{MemoryTaskStore, MemoryAgentStore};

use agentflow_agents::{
    PlannerAgent, SchedulerAgent, NixExecutorAgent, FlakeAnalyzerAgent,
    AICodeReviewerAgent, StorageManagerAgent, BuilderAgent, GitSyncAgent,
    QEMUTestAgent, MoeSyncAgent, MoeVerifyAgent, MoeGCAgent,
    GitHubStatusAgent, MatrixNotifierAgent,
};

use crate::error::Result;

/// Spawned agent handle
pub struct SpawnedAgent {
    pub handle: JoinHandle<()>,
    pub definition: AgentDefinition,
}

/// Create an agent box with Tokio mutex (Send-safe)
type ArcAgent = Arc<Mutex<Box<dyn Agent + Send + Sync + 'static>>>;

/// Spawn an agent worker that receives messages from the bus and dispatches them
async fn spawn_agent_worker(
    name: String,
    agent: ArcAgent,
    bus: Arc<InMemoryBus>,
    state_store: Arc<dyn StateStore + Send + Sync>,
) -> Result<SpawnedAgent> {
    let agent_id = format!("{}-{}", name.to_lowercase().replace(' ', "-"), uuid::Uuid::new_v4());
    
    // Get capabilities
    let capabilities: HashSet<String> = {
        let guard = agent.lock().await;
        guard.capabilities().clone()
    };
    
    // Create agent definition
    let definition = AgentDefinition {
        id: agent_id.clone(),
        name: name.clone(),
        agent_type: AgentType::Custom,
        status: AgentStatus::Ready,
        capabilities,
        max_tasks: 10,
        active_tasks: 0,
        resources: None,
        identity: None,
        config: serde_json::json!({}),
        last_heartbeat: Some(chrono::Utc::now()),
        tasks_completed: 0,
        tasks_failed: 0,
    };
    
    // Register agent in state store
    let _ = state_store.register_agent(&definition).await;
    
    // Get a subscription to messages
    let bus_clone: Arc<dyn MessageBus> = bus.clone();
    let mut stream = bus_clone.subscribe("agents").await.unwrap();
    
    // Clone state_store for the async block
    let store_clone = state_store.clone();
    
    // Spawn agent worker
    let def_clone = definition.clone();
    let handle = tokio::spawn(async move {
        println!("  ✅ Agent {}: started", name);
        
        // Create a minimal context with state store
        let dummy_ctx = AgentContext::new(
            mpsc::channel(1).0,
            def_clone,
            None,
            Some(store_clone.clone()),
        );
        
        loop {
            match stream.next().await {
                Some(message) => {
                    // Process message - lock, handle, unlock (all Send-safe with Tokio Mutex)
                    {
                        let mut guard = agent.lock().await;
                        if let Err(e) = guard.handle_message(message, &dummy_ctx).await {
                            eprintln!("  ❌ Agent {} error: {}", name, e);
                        }
                    }
                }
                None => {
                    println!("  ⚠️  Agent {}: bus disconnected", name);
                    break;
                }
            }
        }
        
        println!("  🛑 Agent {}: stopped", name);
    });
    
    Ok(SpawnedAgent {
        handle,
        definition,
    })
}

/// Helper to create an agent with Tokio Mutex
fn arc_agent(agent: impl Agent + Send + Sync + 'static) -> ArcAgent {
    Arc::new(Mutex::new(Box::new(agent)))
}

/// Spawn all agents as background workers
pub async fn spawn_all_agents(
    bus: Arc<InMemoryBus>,
    task_store: Option<Arc<dyn TaskStore + Send + Sync>>,
    state_store: Option<Arc<dyn StateStore + Send + Sync>>,
) -> Result<Vec<SpawnedAgent>> {
    let mut agents = Vec::new();
    
    
    // Use provided stores or create new ones
    let task_store = task_store.unwrap_or_else(|| Arc::new(MemoryTaskStore::default()));
    let state_store = state_store.unwrap_or_else(|| Arc::new(MemoryAgentStore::default()));
    
    // Planner (2-arg)
    println!("  🔄 Creating PlannerAgent...");
    let agent = arc_agent(PlannerAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
    ));
    agents.push(spawn_agent_worker("PlannerAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // Scheduler (3-arg)
    println!("  🔄 Creating SchedulerAgent...");
    let agent = arc_agent(SchedulerAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        state_store.clone(),
    ));
    agents.push(spawn_agent_worker("SchedulerAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // NixExecutor (3-arg: sender, task_store, system)
    println!("  🔄 Creating NixExecutorAgent...");
    let agent = arc_agent(NixExecutorAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        "x86_64-linux".to_string(),
    ));
    agents.push(spawn_agent_worker("NixExecutorAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // FlakeAnalyzer (2-arg)
    println!("  🔄 Creating FlakeAnalyzerAgent...");
    let agent = arc_agent(FlakeAnalyzerAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
    ));
    agents.push(spawn_agent_worker("FlakeAnalyzerAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // AICodeReviewer (4-arg: id, config, sender, task_store)
    println!("  🔄 Creating AICodeReviewerAgent...");
    let config = agentflow_agents::ai_code_reviewer::AIReviewerConfig::default();
    let agent = arc_agent(AICodeReviewerAgent::new(
        "ai-code-reviewer-1".to_string(),
        config,
        mpsc::channel(10000).0,
        task_store.clone(),
    ));
    agents.push(spawn_agent_worker("AICodeReviewerAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // StorageManager (3-arg: sender, task_store, config)
    println!("  🔄 Creating StorageManagerAgent...");
    let config = agentflow_agents::storage_manager::StorageConfig::default();
    let agent = arc_agent(StorageManagerAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        config,
    ));
    agents.push(spawn_agent_worker("StorageManagerAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // Builder (3-arg: sender, task_store, config)
    println!("  🔄 Creating BuilderAgent...");
    let agent = arc_agent(BuilderAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        None::<agentflow_agents::builder::BuilderConfig>,
    ));
    agents.push(spawn_agent_worker("BuilderAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // GitSync (4-arg: sender, task_store, state_store, config)
    println!("  🔄 Creating GitSyncAgent...");
    let agent = arc_agent(GitSyncAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        state_store.clone(),
        None::<agentflow_agents::git_sync::GitSyncConfig>,
    ));
    agents.push(spawn_agent_worker("GitSyncAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // QEMUTest (4-arg)
    println!("  🔄 Creating QEMUTestAgent...");
    let agent = arc_agent(QEMUTestAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        state_store.clone(),
        None::<agentflow_agents::qemu_test::QemuTestConfig>,
    ));
    agents.push(spawn_agent_worker("QEMUTestAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // MoeSync (4-arg)
    println!("  🔄 Creating MoeSyncAgent...");
    let agent = arc_agent(MoeSyncAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        state_store.clone(),
        None::<agentflow_agents::moe_sync::MoeSyncConfig>,
    ));
    agents.push(spawn_agent_worker("MoeSyncAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // MoeVerify (4-arg)
    println!("  🔄 Creating MoeVerifyAgent...");
    let agent = arc_agent(MoeVerifyAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        state_store.clone(),
        None::<agentflow_agents::moe_verify::MoeVerifyConfig>,
    ));
    agents.push(spawn_agent_worker("MoeVerifyAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // MoeGC (4-arg)
    println!("  🔄 Creating MoeGCAgent...");
    let agent = arc_agent(MoeGCAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        state_store.clone(),
        None::<agentflow_agents::moe_gc::MoeGCConfig>,
    ));
    agents.push(spawn_agent_worker("MoeGCAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // GitHubStatus (4-arg)
    println!("  🔄 Creating GitHubStatusAgent...");
    let config = agentflow_agents::github_status::GitHubConfig::default();
    let agent = arc_agent(GitHubStatusAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        state_store.clone(),
        Some(config),
    ));
    agents.push(spawn_agent_worker("GitHubStatusAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    // MatrixNotifier (4-arg)
    println!("  🔄 Creating MatrixNotifierAgent...");
    let config = agentflow_agents::matrix_notifier::MatrixConfig::default();
    let agent = arc_agent(MatrixNotifierAgent::new(
        mpsc::channel(10000).0,
        task_store.clone(),
        state_store.clone(),
        Some(config),
    ));
    agents.push(spawn_agent_worker("MatrixNotifierAgent".to_string(), agent, bus.clone(), state_store.clone()).await?);
    
    println!("✅ All 14 agents spawned successfully!");
    
    Ok(agents)
}
