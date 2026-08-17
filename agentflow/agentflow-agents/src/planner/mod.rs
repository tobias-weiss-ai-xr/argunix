//! Planner Agent - Creates task DAGs from incoming requests

use agentflow_core::{
    Agent, AgentContext, AgentDefinition, AgentStatus, AgentType, AgentMessage,
    Result, TaskDefinition, TaskType, TaskStatus,
};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Planner Agent - Responsible for analyzing requests and creating task DAGs
pub struct PlannerAgent {
    /// Agent definition
    definition: AgentDefinition,
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
}

impl PlannerAgent {
    /// Create a new PlannerAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    ) -> Self {
        Self {
            definition: AgentDefinition {
                id: "planner-1".to_string(),
                name: "Planner Agent".to_string(),
                agent_type: AgentType::Planner,
                status: AgentStatus::Ready,
                capabilities: HashSet::from([
                    "task-planning".to_string(),
                    "dag-building".to_string(),
                    "flake-analysis".to_string(),
                ]),
                ..Default::default()
            },
            sender,
            task_store,
        }
    }
    
    /// Analyze a flake and create tasks for all outputs
    async fn analyze_and_plan(&self, flake_url: String, flake_ref: Option<String>, system: Option<String>) -> Result<Vec<TaskDefinition>> {
        // In a real implementation, this would call argunix's flake analyzer
        // For now, we create a simple task DAG
        
        let system = system.unwrap_or_else(|| "x86_64-linux".to_string());
        
        // Create eval task
        let eval_task = TaskDefinition {
            id: format!("eval-{}-{}", flake_url, system),
            task_type: TaskType::NixEval,
            status: TaskStatus::Pending,
            priority: 80,
            created_at: chrono::Utc::now(),
            flake_url: Some(flake_url.clone()),
            flake_ref: flake_ref.clone(),
            system: Some(system.clone()),
            targets: Some(vec![]),
            ..Default::default()
        };
        
        // Create build tasks for common outputs
        let build_targets = vec![
            "packages.default",
            "checks.all",
            "devShells.default",
        ];
        
        let mut build_tasks = Vec::new();
        for target in build_targets {
            let task = TaskDefinition {
                id: format!("build-{}-{}-{}", flake_url, system, target),
                task_type: TaskType::NixBuild,
                status: TaskStatus::Pending,
                priority: 60,
                created_at: chrono::Utc::now(),
                flake_url: Some(flake_url.clone()),
                flake_ref: flake_ref.clone(),
                system: Some(system.clone()),
                targets: Some(vec![target.to_string()]),
                depends_on: Some(vec![eval_task.id.clone()]),
                ..Default::default()
            };
            build_tasks.push(task);
        }
        
        let mut all_tasks = vec![eval_task];
        all_tasks.extend(build_tasks);
        
        Ok(all_tasks)
    }
}

#[async_trait::async_trait]
impl Agent for PlannerAgent {
    fn name(&self) -> &str {
        &self.definition.name
    }
    
    fn agent_type(&self) -> AgentType {
        self.definition.agent_type.clone()
    }
    
    fn capabilities(&self) -> &HashSet<String> {
        &self.definition.capabilities
    }
    
    async fn handle_message(&mut self, message: AgentMessage, _ctx: &AgentContext) -> Result<()> {
        match message {
            AgentMessage::SubmitTask(task) => {
                // Check if this is a flake-based task
                if task.flake_url.is_some() {
                    let flake_url = task.flake_url.clone().unwrap();
                    let tasks = self.analyze_and_plan(
                        flake_url,
                        task.flake_ref.clone(),
                        task.system.clone(),
                    ).await?;
                    
                    // Store tasks
                    for task_def in &tasks {
                        self.task_store.create_task(task_def).await?;
                    }
                    
                    // Send tasks to scheduler
                    let message = AgentMessage::SubmitTask(tasks[0].clone());
                    self.sender.send(message).await?;
                    
                    // Submit remaining tasks
                    for task_def in tasks.iter().skip(1) {
                        let message = AgentMessage::SubmitTask(task_def.clone());
                        self.sender.send(message).await?;
                    }
                } else {
                    // Forward other task types directly
                    self.sender.send(AgentMessage::SubmitTask(task)).await?;
                }
            }
            
            AgentMessage::AnalyzeFlake { flake_url, flake_ref, task_id: _ } => {
                let tasks = self.analyze_and_plan(flake_url, flake_ref, None).await?;
                
                // Store tasks
                for task_def in &tasks {
                    self.task_store.create_task(task_def).await?;
                }
                
                // Notify of completion
                // In real implementation, return analysis results
            }
            
            AgentMessage::EvaluateFlake { flake_url, flake_ref, system, targets, task_id: _ } => {
                let tasks = self.analyze_and_plan(flake_url, Some(flake_ref), Some(system)).await?;
                
                // Filter by targets if specified
                let filtered_tasks: Vec<TaskDefinition> = if targets.is_empty() {
                    tasks
                } else {
                    tasks.into_iter()
                        .filter(|t| {
                            if let Some(tgts) = &t.targets {
                                targets.iter().any(|target| tgts.contains(target))
                            } else {
                                false
                            }
                        })
                        .collect()
                };
                
                for task_def in &filtered_tasks {
                    self.task_store.create_task(task_def).await?;
                    self.sender.send(AgentMessage::SubmitTask(task_def.clone())).await?;
                }
            }
            
            _ => {
                // Ignore other message types or forward to appropriate agent
            }
        }
        
        Ok(())
    }
    
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> {
        println!("PlannerAgent started");
        Ok(())
    }
    
    fn status(&self) -> AgentStatus {
        self.definition.status.clone()
    }
}

impl Default for PlannerAgent {
    fn default() -> Self {
        // This won't work without a sender, but provides a default
        // In practice, use new() with proper parameters
        panic!("Use PlannerAgent::new() instead of Default::default()");
    }
}
