//! AgentFlow Storage - Persistent storage implementations

use agentflow_core::{
    AgentDefinition, TaskDefinition, TaskFilter, TaskStatus,
    agent::{TaskStore, StateStore, TaskUpdate},
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub mod sqlite;
pub mod redis;
pub mod filesystem;

/// In-memory storage for development and testing
pub struct MemoryStorage {
    tasks: Arc<RwLock<HashMap<String, TaskDefinition>>>,
    agents: Arc<RwLock<HashMap<String, AgentDefinition>>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            agents: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl TaskStore for MemoryStorage {
    async fn create_task(&self, task: &TaskDefinition) -> agentflow_core::Result<()> {
        let mut tasks = self.tasks.write().await;
        tasks.insert(task.id.clone(), task.clone());
        Ok(())
    }
    
    async fn get_task(&self, task_id: &str) -> agentflow_core::Result<Option<TaskDefinition>> {
        let tasks = self.tasks.read().await;
        Ok(tasks.get(task_id).cloned())
    }
    
    async fn list_tasks(&self, filter: Option<TaskFilter>) -> agentflow_core::Result<Vec<TaskDefinition>> {
        let tasks = self.tasks.read().await;
        
        let tasks: Vec<TaskDefinition> = if let Some(f) = filter {
            tasks.values().filter(|t| self.passes_filter(t, &f)).cloned().collect()
        } else {
            tasks.values().cloned().collect()
        };
        
        Ok(tasks)
    }
    
    async fn update_task(&self, task_id: &str, update: TaskUpdate) -> agentflow_core::Result<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(task_id) {
            if let Some(status) = update.status {
                task.status = status;
            }
            if let Some(priority) = update.priority {
                task.priority = priority;
            }
            if let Some(started_at) = update.started_at {
                task.started_at = Some(started_at);
            }
            if let Some(completed_at) = update.completed_at {
                task.completed_at = Some(completed_at);
            }
            if let Some(result) = update.result {
                task.result = Some(result);
            }
            if let Some(error) = update.error {
                task.error = Some(error);
            }
            if let Some(flake_url) = update.flake_url {
                task.flake_url = Some(flake_url);
            }
            if let Some(flake_ref) = update.flake_ref {
                task.flake_ref = Some(flake_ref);
            }
            if let Some(outputs) = update.outputs {
                task.outputs = outputs;
            }
        }
        Ok(())
    }
    
    async fn delete_task(&self, task_id: &str) -> agentflow_core::Result<()> {
        let mut tasks = self.tasks.write().await;
        tasks.remove(task_id);
        Ok(())
    }
    
    async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> agentflow_core::Result<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.status = status;
        }
        Ok(())
    }
}

impl MemoryStorage {
    fn passes_filter(&self, task: &TaskDefinition, filter: &TaskFilter) -> bool {
        if let Some(ref statuses) = filter.status {
            if !statuses.contains(&task.status) {
                return false;
            }
        }
        if let Some(ref types) = filter.task_type {
            if !types.contains(&task.task_type) {
                return false;
            }
        }
        if let Some(min) = filter.priority_min {
            if task.priority < min {
                return false;
            }
        }
        if let Some(max) = filter.priority_max {
            if task.priority > max {
                return false;
            }
        }
        true
    }
}

#[async_trait]
impl StateStore for MemoryStorage {
    async fn register_agent(&self, agent: &AgentDefinition) -> agentflow_core::Result<()> {
        let mut agents = self.agents.write().await;
        agents.insert(agent.id.clone(), agent.clone());
        Ok(())
    }
    
    async fn deregister_agent(&self, agent_id: &str) -> agentflow_core::Result<()> {
        let mut agents = self.agents.write().await;
        agents.remove(agent_id);
        Ok(())
    }
    
    async fn get_agent(&self, agent_id: &str) -> agentflow_core::Result<Option<AgentDefinition>> {
        let agents = self.agents.read().await;
        Ok(agents.get(agent_id).cloned())
    }
    
    async fn list_agents(&self) -> agentflow_core::Result<Vec<AgentDefinition>> {
        let agents = self.agents.read().await;
        Ok(agents.values().cloned().collect())
    }
    
    async fn update_agent(&self, agent_id: &str, agent: &AgentDefinition) -> agentflow_core::Result<()> {
        let mut agents = self.agents.write().await;
        agents.insert(agent_id.to_string(), agent.clone());
        Ok(())
    }
}

/// Storage factory trait
pub trait StorageFactory: Send + Sync {
    fn create_task_store(&self) -> Arc<dyn TaskStore + Send + Sync>;
    fn create_state_store(&self) -> Arc<dyn StateStore + Send + Sync>;
}

/// In-memory storage factory
pub struct MemoryStorageFactory;

impl StorageFactory for MemoryStorageFactory {
    fn create_task_store(&self) -> Arc<dyn TaskStore + Send + Sync> {
        Arc::new(MemoryStorage::new())
    }
    
    fn create_state_store(&self) -> Arc<dyn StateStore + Send + Sync> {
        Arc::new(MemoryStorage::new())
    }
}

impl Default for MemoryStorageFactory {
    fn default() -> Self {
        Self
    }
}
