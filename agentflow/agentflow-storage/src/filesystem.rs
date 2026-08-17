//! Filesystem-based storage for tasks and agents

use agentflow_core::{
    AgentDefinition, TaskDefinition, TaskFilter, TaskStatus, AgentFlowError,
    agent::{TaskStore, StateStore, TaskUpdate},
    Result,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use serde_json;

/// Filesystem storage configuration
#[derive(Debug, Clone)]
pub struct FilesystemConfig {
    pub base_path: PathBuf,
    pub tasks_dir: String,
    pub agents_dir: String,
}

impl Default for FilesystemConfig {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("./agentflow-data"),
            tasks_dir: "tasks".to_string(),
            agents_dir: "agents".to_string(),
        }
    }
}

/// Filesystem storage implementation
pub struct FilesystemStorage {
    config: FilesystemConfig,
}

impl FilesystemStorage {
    pub fn new(config: FilesystemConfig) -> Self {
        Self { config }
    }
    
    pub async fn initialize(&self) -> Result<()> {
        // Create directories if they don't exist
        let tasks_dir = self.tasks_path();
        let agents_dir = self.agents_path();
        
        if !tasks_dir.exists() {
            fs::create_dir_all(&tasks_dir).await
                .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        }
        
        if !agents_dir.exists() {
            fs::create_dir_all(&agents_dir).await
                .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        }
        
        Ok(())
    }
    
    fn tasks_path(&self) -> PathBuf {
        self.config.base_path.join(&self.config.tasks_dir)
    }
    
    fn agents_path(&self) -> PathBuf {
        self.config.base_path.join(&self.config.agents_dir)
    }
    
    fn task_path(&self, task_id: &str) -> PathBuf {
        self.tasks_path().join(format!("{}.json", task_id))
    }
    
    fn agent_path(&self, agent_id: &str) -> PathBuf {
        self.agents_path().join(format!("{}.json", agent_id))
    }
    
    async fn read_json<T: serde::de::DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        if !path.exists() {
            return Ok(None);
        }
        
        let mut file = File::open(path).await
            .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        
        let mut contents = String::new();
        file.read_to_string(&mut contents).await
            .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        
        let value: T = serde_json::from_str(&contents)
            .map_err(|e| AgentFlowError::Serialization(e.to_string()))?;
        
        Ok(Some(value))
    }
    
    async fn write_json<T: serde::Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let json = serde_json::to_string_pretty(value)
            .map_err(|e| AgentFlowError::Serialization(e.to_string()))?;
        
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
            .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        
        file.write_all(json.as_bytes()).await
            .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        
        Ok(())
    }
    
    async fn list_json_files(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut entries = fs::read_dir(dir).await
            .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await
            .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?
        {
            if entry.path().extension().map(|s| s == "json").unwrap_or(false) {
                files.push(entry.path());
            }
        }
        
        Ok(files)
    }
}

#[async_trait]
impl TaskStore for FilesystemStorage {
    async fn create_task(&self, task: &TaskDefinition) -> Result<()> {
        let path = self.task_path(&task.id);
        self.write_json(&path, task).await
    }
    
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskDefinition>> {
        let path = self.task_path(task_id);
        self.read_json::<TaskDefinition>(&path).await
    }
    
    async fn list_tasks(&self, filter: Option<TaskFilter>) -> Result<Vec<TaskDefinition>> {
        let dir = self.tasks_path();
        let files = self.list_json_files(&dir).await?;
        
        let mut tasks = Vec::new();
        for file in files {
            if let Some(task) = self.read_json::<TaskDefinition>(&file).await? {
                if let Some(ref f) = filter {
                    if self.passes_filter(&task, f) {
                        tasks.push(task);
                    }
                } else {
                    tasks.push(task);
                }
            }
        }
        
        Ok(tasks)
    }
    
    async fn update_task(&self, task_id: &str, update: TaskUpdate) -> Result<()> {
        let path = self.task_path(task_id);
        
        // Get existing task
        if let Some(mut task) = self.read_json::<TaskDefinition>(&path).await? {
            // Apply updates
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
            
            // Write back
            self.write_json(&path, &task).await?;
        }
        
        Ok(())
    }
    
    async fn delete_task(&self, task_id: &str) -> Result<()> {
        let path = self.task_path(task_id);
        if path.exists() {
            fs::remove_file(&path).await
                .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        }
        Ok(())
    }
    
    async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> Result<()> {
        let update = TaskUpdate {
            status: Some(status),
            ..Default::default()
        };
        self.update_task(task_id, update).await
    }
}

impl FilesystemStorage {
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
impl StateStore for FilesystemStorage {
    async fn register_agent(&self, agent: &AgentDefinition) -> Result<()> {
        let path = self.agent_path(&agent.id);
        self.write_json(&path, agent).await
    }
    
    async fn deregister_agent(&self, agent_id: &str) -> Result<()> {
        let path = self.agent_path(agent_id);
        if path.exists() {
            fs::remove_file(&path).await
                .map_err(|e| AgentFlowError::Filesystem(e.to_string()))?;
        }
        Ok(())
    }
    
    async fn get_agent(&self, agent_id: &str) -> Result<Option<AgentDefinition>> {
        let path = self.agent_path(agent_id);
        self.read_json::<AgentDefinition>(&path).await
    }
    
    async fn list_agents(&self) -> Result<Vec<AgentDefinition>> {
        let dir = self.agents_path();
        let files = self.list_json_files(&dir).await?;
        
        let mut agents = Vec::new();
        for file in files {
            if let Some(agent) = self.read_json::<AgentDefinition>(&file).await? {
                agents.push(agent);
            }
        }
        
        Ok(agents)
    }
    
    async fn update_agent(&self, agent_id: &str, agent: &AgentDefinition) -> Result<()> {
        let path = self.agent_path(agent_id);
        self.write_json(&path, agent).await
    }
}

/// Filesystem storage factory
pub struct FilesystemStorageFactory {
    config: FilesystemConfig,
}

impl FilesystemStorageFactory {
    pub fn new(config: FilesystemConfig) -> Self {
        Self { config }
    }
}

impl super::StorageFactory for FilesystemStorageFactory {
    fn create_task_store(&self) -> Arc<dyn TaskStore + Send + Sync> {
        Arc::new(FilesystemStorage::new(self.config.clone()))
    }
    
    fn create_state_store(&self) -> Arc<dyn StateStore + Send + Sync> {
        Arc::new(FilesystemStorage::new(self.config.clone()))
    }
}
