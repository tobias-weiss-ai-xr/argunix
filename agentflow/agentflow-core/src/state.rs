use crate::{TaskDefinition, TaskFilter, TaskUpdate};
use crate::agent::AgentDefinition;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use chrono::Utc;

/// In-memory task store for development and testing
#[derive(Debug, Clone, Default)]
pub struct MemoryTaskStore {
    tasks: Arc<parking_lot::RwLock<HashMap<String, TaskDefinition>>>,
}

#[async_trait]
impl crate::agent::TaskStore for MemoryTaskStore {
    async fn create_task(&self, task: &TaskDefinition) -> crate::Result<TaskDefinition> {
        let mut tasks = self.tasks.write();
        tasks.insert(task.id.clone(), task.clone());
        Ok(task.clone())
    }
    
    async fn get_task(&self, id: &str) -> crate::Result<Option<TaskDefinition>> {
        let tasks = self.tasks.read();
        Ok(tasks.get(id).cloned())
    }
    
    async fn update_task(&self, id: &str, update: TaskUpdate) -> crate::Result<TaskDefinition> {
        let mut tasks = self.tasks.write();
        if let Some(task) = tasks.get_mut(id) {
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
            if let Some(metadata) = update.metadata {
                task.metadata.extend(metadata);
            }
            Ok(task.clone())
        } else {
            Err(crate::AgentFlowError::NotFound(id.to_string()))
        }
    }
    
    async fn list_tasks(&self, filter: Option<TaskFilter>) -> crate::Result<Vec<TaskDefinition>> {
        let tasks = self.tasks.read();
        let mut result: Vec<TaskDefinition> = tasks.values().cloned().collect();
        
        if let Some(f) = filter {
            if let Some(statuses) = f.status {
                result.retain(|t| statuses.contains(&t.status));
            }
            if let Some(types) = f.task_type {
                result.retain(|t| types.contains(&t.task_type));
            }
            if let Some(min_priority) = f.priority_min {
                result.retain(|t| t.priority >= min_priority);
            }
            if let Some(max_priority) = f.priority_max {
                result.retain(|t| t.priority <= max_priority);
            }
            if let Some(flake_url) = &f.flake_url {
                result.retain(|t| t.flake_url.as_deref() == Some(flake_url));
            }
            if let Some(limit) = f.limit {
                result.truncate(limit);
            }
            if let Some(offset) = f.offset {
                if offset < result.len() {
                    result.drain(..offset);
                } else {
                    result.clear();
                }
            }
        }
        
        Ok(result)
    }
    
    async fn delete_task(&self, id: &str) -> crate::Result<()> {
        let mut tasks = self.tasks.write();
        tasks.remove(id);
        Ok(())
    }
}

/// In-memory agent store
#[derive(Debug, Clone, Default)]
pub struct MemoryAgentStore {
    agents: Arc<parking_lot::RwLock<HashMap<String, AgentDefinition>>>,
}

#[async_trait]
impl crate::agent::StateStore for MemoryAgentStore {
    async fn get_agent(&self, id: &str) -> crate::Result<Option<AgentDefinition>> {
        let agents = self.agents.read();
        Ok(agents.get(id).cloned())
    }
    
    async fn register_agent(&self, agent: &AgentDefinition) -> crate::Result<()> {
        let mut agents = self.agents.write();
        let mut agent = agent.clone();
        agent.last_heartbeat = Some(Utc::now());
        agents.insert(agent.id.clone(), agent);
        Ok(())
    }
    
    async fn deregister_agent(&self, id: &str) -> crate::Result<()> {
        let mut agents = self.agents.write();
        agents.remove(id);
        Ok(())
    }
    
    async fn list_agents(&self) -> crate::Result<Vec<AgentDefinition>> {
        let agents = self.agents.read();
        Ok(agents.values().cloned().collect())
    }
}

/// System state holding task and agent stores
#[derive(Clone)]
pub struct SystemState {
    pub task_store: Arc<dyn crate::agent::TaskStore + Send + Sync>,
    pub agent_store: Arc<dyn crate::agent::StateStore + Send + Sync>,
}

impl Default for SystemState {
    fn default() -> Self {
        Self {
            task_store: Arc::new(MemoryTaskStore::default()),
            agent_store: Arc::new(MemoryAgentStore::default()),
        }
    }
}

impl SystemState {
    /// Create a new SystemState with default in-memory stores
    pub fn new() -> Self {
        Self::default()
    }
    
    /// Create with custom stores
    pub fn with_stores(
        task_store: Arc<dyn crate::agent::TaskStore + Send + Sync>,
        agent_store: Arc<dyn crate::agent::StateStore + Send + Sync>,
    ) -> Self {
        Self { task_store, agent_store }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{TaskDefinition, TaskStatus, TaskType};
    use crate::agent::{AgentDefinition, AgentType, AgentStatus, TaskStore, StateStore};
    use chrono::Utc;
    
    #[tokio::test]
    async fn test_memory_task_store() {
        let store = MemoryTaskStore::default();
        
        // Create a task
        let task = TaskDefinition {
            id: "test-1".to_string(),
            task_type: TaskType::NixEval,
            status: TaskStatus::Pending,
            priority: 50,
            created_at: Utc::now(),
            ..Default::default()
        };
        
        // Store it
        let created = store.create_task(&task).await.unwrap();
        assert_eq!(created.id, "test-1");
        
        // Retrieve it
        let retrieved = store.get_task("test-1").await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id, "test-1");
        
        // List tasks
        let tasks = store.list_tasks(None).await.unwrap();
        assert_eq!(tasks.len(), 1);
        
        // Delete it
        store.delete_task("test-1").await.unwrap();
        let deleted = store.get_task("test-1").await.unwrap();
        assert!(deleted.is_none());
    }
    
    #[tokio::test]
    async fn test_memory_agent_store() {
        let store = MemoryAgentStore::default();
        
        // Create an agent
        let agent = AgentDefinition {
            id: "agent-1".to_string(),
            name: "Test Agent".to_string(),
            agent_type: AgentType::Planner,
            status: AgentStatus::Ready,
            ..Default::default()
        };
        
        // Register it
        store.register_agent(&agent).await.unwrap();
        
        // Retrieve it
        let retrieved = store.get_agent("agent-1").await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "Test Agent");
        
        // List agents
        let agents = store.list_agents().await.unwrap();
        assert_eq!(agents.len(), 1);
        
        // Deregister it
        store.deregister_agent("agent-1").await.unwrap();
        let deleted = store.get_agent("agent-1").await.unwrap();
        assert!(deleted.is_none());
    }
    
    #[tokio::test]
    async fn test_task_filter() {
        let store = MemoryTaskStore::default();
        
        // Create tasks with different statuses
        for i in 0..5 {
            let task = TaskDefinition {
                id: format!("task-{}", i),
                task_type: TaskType::NixEval,
                status: if i % 2 == 0 { TaskStatus::Pending } else { TaskStatus::Running },
                priority: i as u8 * 10,
                created_at: Utc::now(),
                ..Default::default()
            };
            store.create_task(&task).await.unwrap();
        }
        
        // Filter by status
        let filter = TaskFilter {
            status: Some(vec![TaskStatus::Pending]),
            ..Default::default()
        };
        let pending = store.list_tasks(Some(filter)).await.unwrap();
        assert_eq!(pending.len(), 3); // tasks 0, 2, 4
        
        // Filter by priority
        let filter = TaskFilter {
            priority_min: Some(10),
            priority_max: Some(30),
            ..Default::default()
        };
        let in_range = store.list_tasks(Some(filter)).await.unwrap();
        assert_eq!(in_range.len(), 3); // tasks 1, 2, 3 (priorities 10, 20, 30)
    }
}
