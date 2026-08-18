//! Dispatch a test task using AgentFlow message bus
//! This demonstrates how to use the AgentFlow framework programmatically

use std::sync::Arc;
use tokio::sync::mpsc;
use agentflow_core::{
    Agent,
    AgentMessage,
    MessageBus,
    InMemoryBus,
    SystemState,
    TaskDefinition,
    TaskStatus,
    TaskType,
    memory::MemoryTaskStore,
};
use agentflow_agents::{
    PlannerAgent,
    SchedulerAgent,
    GitHubStatusAgent,
    MatrixNotifierAgent,
};

// Mock StateStore implementation
use agentflow_core::AgentFlowError;
use agentflow_core::Result;
use async_trait::async_trait;

struct SimpleStateStore;

#[async_trait]
impl agentflow_core::agent::StateStore for SimpleStateStore {
    async fn get_agent(&self, _id: &str) -> Result<Option<agentflow_core::agent::AgentDefinition>> {
        Ok(None)
    }
    
    async fn register_agent(&self, _agent: &agentflow_core::agent::AgentDefinition) -> Result<()> {
        Ok(())
    }
    
    async fn deregister_agent(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    
    async fn list_agents(&self) -> Result<Vec<agentflow_core::agent::AgentDefinition>> {
        Ok(vec![])
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 Starting AgentFlow notification test dispatch...\n");
    
    // Create message bus
    let bus = Arc::new(InMemoryBus::new());
    let bus_sender = bus.clone();
    
    // Create task store
    let task_store = Arc::new(MemoryTaskStore::default());
    let state_store = Arc::new(SimpleStateStore);
    
    // Spawn agents
    let b = bus.clone();
    let t = task_store.clone();
    let s = state_store.clone();
    
    // Create GitHubStatusAgent
    let github_config = agentflow_agents::github_status::GitHubConfig::default();
    let github_agent = Arc::new(GitHubStatusAgent::new(
        b.clone(),
        t.clone(),
        s.clone(),
        Some(github_config)
    )) as Arc<dyn Agent + Send + Sync>;
    
    // Create MatrixNotifierAgent
    let matrix_config = agentflow_agents::matrix_notifier::MatrixConfig::default();
    let matrix_agent = Arc::new(MatrixNotifierAgent::new(
        b.clone(),
        t.clone(),
        s.clone(),
        Some(matrix_config)
    )) as Arc<dyn Agent + Send + Sync>;
    
    // Register agents with state
    let system_state = SystemState::new(bus_sender.clone(), task_store.clone(), state_store.clone());
    
    // Create test task: Post GitHub Status
    let task_def = TaskDefinition {
        id: uuid::Uuid::new_v4().to_string(),
        task_type: TaskType::PostGitHubStatus,
        payload: serde_json::json!({
            "owner": "tobias-weiss-ai-xr",
            "repo": "argunix",
            "sha": "abc123",
            "state": "success",
            "description": "Test notification from AgentFlow",
            "context": "agentflow-test"
        }),
        priority: 50,
        status: TaskStatus::Pending,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        retries: 0,
        max_retries: 3,
        dependencies: vec![],
    };
    
    println!("✅ Created test task:");
    println!("   ID: {}", task_def.id);
    println!("   Type: {:?}", task_def.task_type);
    println!();
    
    // Store task
    task_store.create_task(&task_def).await?;
    println!("✅ Task stored in MemoryTaskStore");
    
    // Send message to GitHubStatusAgent
    let message = AgentMessage::Task(agentflow_core::TaskMessage {
        task: task_def,
        agent_id: "github-status-1".to_string(),
    });
    
    bus_sender.send(message).await?;
    println!("✅ Task message sent to GitHubStatusAgent");
    
    // Create test task: Send Matrix Notification
    let task_def2 = TaskDefinition {
        id: uuid::Uuid::new_v4().to_string(),
        task_type: TaskType::SendMatrixNotification,
        payload: serde_json::json!({
            "room_id": "!test:matrix.org",
            "message": "Hello from AgentFlow!",
            "html-enabled": true
        }),
        priority: 50,
        status: TaskStatus::Pending,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        retries: 0,
        max_retries: 3,
        dependencies: vec![],
    };
    
    println!("✅ Created Matrix notification task:");
    println!("   ID: {}", task_def2.id);
    println!("   Type: {:?}", task_def2.task_type);
    println!();
    
    task_store.create_task(&task_def2).await?;
    println!("✅ Matrix task stored in MemoryTaskStore");
    
    let message2 = AgentMessage::Task(agentflow_core::TaskMessage {
        task: task_def2,
        agent_id: "matrix-notifier-1".to_string(),
    });
    
    bus_sender.send(message2).await?;
    println!("✅ Matrix notification message sent to MatrixNotifierAgent");
    
    println!("\n🎉 Test dispatch complete!");
    println!("   (Note: Agents need to be running to process these messages)");
    
    Ok(())
}
