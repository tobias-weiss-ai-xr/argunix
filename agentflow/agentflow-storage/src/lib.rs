//! AgentFlow Storage - Persistent storage implementations (stub for now)
//!
//! This crate will contain various storage backends for:
//! - Tasks
//! - Agents  
//! - State
//!
//! For now, we re-export the core storage traits and use in-memory storage.

pub use agentflow_core::{
    agent::{TaskStore, StateStore, TaskUpdate, TaskFilter},
    TaskDefinition, AgentDefinition,
    Result, AgentFlowError,
};

// Re-export for convenience
pub mod memory {
    use agentflow_core::{AgentDefinition, TaskDefinition, TaskFilter, TaskStatus, Result, AgentFlowError};
    use agentflow_core::agent::{TaskStore, StateStore, TaskUpdate};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// In-memory task store
    pub struct MemoryTaskStore {
        tasks: Arc<RwLock<HashMap<String, TaskDefinition>>>,
    }
    
    impl MemoryTaskStore {
        pub fn new() -> Self {
            Self {
                tasks: Arc::new(RwLock::new(HashMap::new())),
            }
        }
    }
    
    #[async_trait]
    impl TaskStore for MemoryTaskStore {
        async fn create_task(&self, task: &TaskDefinition) -> Result<TaskDefinition> {
            let mut tasks = self.tasks.write().await;
            tasks.insert(task.id.clone(), task.clone());
            Ok(task.clone())
        }
        
        async fn get_task(&self, id: &str) -> Result<Option<TaskDefinition>> {
            let tasks = self.tasks.read().await;
            Ok(tasks.get(id).cloned())
        }
        
        async fn update_task(&self, id: &str, update: TaskUpdate) -> Result<TaskDefinition> {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.get_mut(id) {
                let mut updated = task.clone();
                if let Some(status) = update.status {
                    updated.status = status;
                }
                if let Some(priority) = update.priority {
                    updated.priority = priority;
                }
                *task = updated.clone();
                Ok(updated)
            } else {
                Err(AgentFlowError::Generic(format!("Task {} not found", id)))
            }
        }
        
        async fn list_tasks(&self, _filter: Option<TaskFilter>) -> Result<Vec<TaskDefinition>> {
            let tasks = self.tasks.read().await;
            Ok(tasks.values().cloned().collect())
        }
        
        async fn delete_task(&self, id: &str) -> Result<()> {
            let mut tasks = self.tasks.write().await;
            tasks.remove(id);
            Ok(())
        }
    }
    
    impl Default for MemoryTaskStore {
        fn default() -> Self {
            Self::new()
        }
    }
    
    /// In-memory state store
    pub struct MemoryStateStore {
        agents: Arc<RwLock<HashMap<String, AgentDefinition>>>,
    }
    
    impl MemoryStateStore {
        pub fn new() -> Self {
            Self {
                agents: Arc::new(RwLock::new(HashMap::new())),
            }
        }
    }
    
    #[async_trait]
    impl StateStore for MemoryStateStore {
        async fn get_agent(&self, id: &str) -> Result<Option<AgentDefinition>> {
            let agents = self.agents.read().await;
            Ok(agents.get(id).cloned())
        }
        
        async fn register_agent(&self, agent: &AgentDefinition) -> Result<()> {
            let mut agents = self.agents.write().await;
            agents.insert(agent.id.clone(), agent.clone());
            Ok(())
        }
        
        async fn deregister_agent(&self, id: &str) -> Result<()> {
            let mut agents = self.agents.write().await;
            agents.remove(id);
            Ok(())
        }
        
        async fn list_agents(&self) -> Result<Vec<AgentDefinition>> {
            let agents = self.agents.read().await;
            Ok(agents.values().cloned().collect())
        }
    }
    
    impl Default for MemoryStateStore {
        fn default() -> Self {
            Self::new()
        }
    }
}
