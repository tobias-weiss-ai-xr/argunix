//! Scheduler Agent - Assigns tasks to appropriate agents

use agentflow_core::{
    Agent, AgentContext, AgentDefinition, AgentStatus, AgentType, AgentMessage,
    Result, TaskDefinition, TaskType, TaskStatus,
};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::cmp::Ordering;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Priority queue item for task scheduling
#[derive(Debug, Clone)]
struct QueuedTask {
    task: TaskDefinition,
    priority: u8,
}

impl PartialEq for QueuedTask {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl Eq for QueuedTask {}

impl PartialOrd for QueuedTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueuedTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first
        other.priority.cmp(&self.priority)
            // Then by creation time (older first)
            .then(self.task.created_at.cmp(&other.task.created_at))
    }
}

/// Agent information for scheduling
#[derive(Debug, Clone)]
struct AgentInfo {
    pub id: String,
    pub agent_type: AgentType,
    pub capabilities: HashSet<String>,
    pub max_tasks: u32,
    pub current_tasks: u32,
    pub resources: Option<agentflow_core::task::ResourceRequirements>,
}

/// Scheduler Agent - Assigns tasks to agents based on availability and capabilities
pub struct SchedulerAgent {
    /// Agent definition
    definition: AgentDefinition,
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    /// State store
    state_store: Arc<dyn agentflow_core::agent::StateStore + Send + Sync>,
    /// Priority queue of tasks
    task_queue: BinaryHeap<QueuedTask>,
    /// Map of available agents
    agents: HashMap<String, AgentInfo>,
}

impl SchedulerAgent {
    /// Create a new SchedulerAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
        state_store: Arc<dyn agentflow_core::agent::StateStore + Send + Sync>,
    ) -> Self {
        Self {
            definition: AgentDefinition {
                id: "scheduler-1".to_string(),
                name: "Scheduler Agent".to_string(),
                agent_type: AgentType::Scheduler,
                status: AgentStatus::Ready,
                capabilities: HashSet::from([
                    "task-scheduling".to_string(),
                    "load-balancing".to_string(),
                    "agent-management".to_string(),
                ]),
                ..Default::default()
            },
            sender,
            task_store,
            state_store,
            task_queue: BinaryHeap::new(),
            agents: HashMap::new(),
        }
    }
    
    /// Check if an agent can handle a task
    fn can_handle_task(&self, agent: &AgentInfo, task: &TaskDefinition) -> bool {
        // Check if agent has capacity
        if agent.current_tasks >= agent.max_tasks {
            return false;
        }
        
        // Check capabilities based on task type
        match task.task_type {
            TaskType::NixEval | TaskType::NixBuild | TaskType::NixCheck | TaskType::NixDevShell | TaskType::NixBundle => {
                agent.capabilities.contains("nix-build") ||
                agent.capabilities.contains("nix") ||
                agent.agent_type == AgentType::NixExecutor ||
                agent.agent_type == AgentType::Builder
            }
            TaskType::AICodeReview | TaskType::AIFlakeAnalysis | TaskType::AIPlanGeneration => {
                agent.capabilities.contains("ai-inference") ||
                agent.agent_type == AgentType::AICodeReviewer ||
                agent.agent_type == AgentType::AIFlakeAnalyzer
            }
            TaskType::MoeSync | TaskType::MoeVerify | TaskType::MoeGC => {
                agent.capabilities.contains("moe-storage") ||
                agent.agent_type == AgentType::StorageManager
            }
            TaskType::StoreObject | TaskType::LoadObject | TaskType::CacheCheck | TaskType::CacheUpload | TaskType::CacheCleanup => {
                agent.capabilities.contains("storage") ||
                agent.capabilities.contains("cache") ||
                agent.agent_type == AgentType::StorageManager
            }
            TaskType::SyncRepository | TaskType::PollRepository | TaskType::SetupRepository | 
                TaskType::PollAllRepositories | TaskType::WebhookReceived | TaskType::GetRepositoryStatus => {
                agent.capabilities.contains("git-sync") ||
                agent.agent_type == AgentType::Custom // GitSync will have its own type eventually
            }
            TaskType::ProvisionVM | TaskType::DestroyVM | TaskType::RunTests => {
                agent.capabilities.contains("qemu-testing") ||
                agent.agent_type == AgentType::Custom // QEMUTest will have its own type eventually
            }
            TaskType::CustomCommand | TaskType::MultiTask => {
                true // Any agent can handle generic tasks
            }
        }
    }
    
    /// Find the best agent ID for a task
    fn find_best_agent(&self, task: &TaskDefinition) -> Option<String> {
        self.agents.values()
            .filter(|agent| self.can_handle_task(agent, task))
            .max_by(|a, b| {
                // Prefer less busy agents
                b.current_tasks.cmp(&a.current_tasks)
                    // Prefer agents with matching type
                    .then(
                        match (&task.task_type, &a.agent_type, &b.agent_type) {
                            (TaskType::NixEval, AgentType::NixExecutor, _) => Ordering::Greater,
                            (TaskType::NixEval, _, AgentType::NixExecutor) => Ordering::Less,
                            (TaskType::NixBuild, AgentType::Builder, _) => Ordering::Greater,
                            (TaskType::NixBuild, _, AgentType::Builder) => Ordering::Less,
                            _ => Ordering::Equal,
                        }
                    )
            })
            .map(|agent| agent.id.clone())
    }
    
    /// Schedule pending tasks
    async fn schedule_pending(&mut self) -> Result<()> {
        while let Some(queued) = self.task_queue.peek() {
            let task = queued.task.clone();
            
            if let Some(agent_id) = self.find_best_agent(&task) {
                // Pop from queue
                self.task_queue.pop();
                
                // Update task status
                let update = agentflow_core::agent::TaskUpdate {
                    status: Some(TaskStatus::Scheduled),
                    ..Default::default()
                };
                self.task_store.update_task(&task.id, update).await?;
                
                // Update agent task count
                if let Some(agent_info) = self.agents.get_mut(&agent_id) {
                    agent_info.current_tasks += 1;
                }
                
                // Send task to agent
                let message = AgentMessage::ExecuteTask(task.clone());
                self.sender.send(message).await?;
                
                // Notify that task was scheduled
                let notify = AgentMessage::TaskScheduled {
                    task_id: task.id,
                    agent_id,
                };
                self.sender.send(notify).await?;
            } else {
                // No available agent, wait for next agent registration
                break;
            }
        }
        
        Ok(())
    }
}

#[async_trait::async_trait]
impl Agent for SchedulerAgent {
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
                // Add to queue
                self.task_queue.push(QueuedTask {
                    task: task.clone(),
                    priority: task.priority,
                });
                
                // Store task
                self.task_store.create_task(&task).await?;
                
                // Try to schedule
                self.schedule_pending().await?;
            }
            
            AgentMessage::ExecuteTask(task) => {
                // Forward to appropriate agent
                if let Some(_agent_id) = self.find_best_agent(&task) {
                    let message = AgentMessage::ExecuteTask(task);
                    self.sender.send(message).await?;
                } else {
                    // Requeue
                    self.task_queue.push(QueuedTask {
                        task,
                        priority: 50,
                    });
                }
            }
            
            AgentMessage::TaskResult(result) => {
                // Update task status
                let update = agentflow_core::agent::TaskUpdate {
                    status: Some(result.status),
                    completed_at: result.completed_at,
                    ..Default::default()
                };
                self.task_store.update_task(&result.task_id, update).await?;
                
                // Mark an agent as available
                // Find which agent was working on this task
                // (In real implementation, track task-agent assignments)
            }
            
            AgentMessage::TaskFailed { task_id, error: _error } => {
                // Update task status
                let update = agentflow_core::agent::TaskUpdate {
                    status: Some(TaskStatus::Failed),
                    ..Default::default()
                };
                self.task_store.update_task(&task_id, update).await?;
                
                // Mark agent as available
                // Try to reschedule if retry policy allows
            }
            
            AgentMessage::AgentReady { agent_id } => {
                // Get agent info from state store
                if let Some(agent_def) = self.state_store.get_agent(&agent_id).await? {
                    let info = AgentInfo {
                        id: agent_id.clone(),
                        agent_type: agent_def.agent_type,
                        capabilities: agent_def.capabilities,
                        max_tasks: agent_def.max_tasks,
                        current_tasks: 0,
                        resources: None,
                    };
                    self.agents.insert(agent_id, info);
                    
                    // Try to schedule pending tasks
                    self.schedule_pending().await?;
                }
            }
            
            AgentMessage::AgentBusy { agent_id, task_count } => {
                if let Some(agent) = self.agents.get_mut(&agent_id) {
                    agent.current_tasks = task_count;
                }
            }
            
            AgentMessage::AgentIdle { agent_id } => {
                if let Some(agent) = self.agents.get_mut(&agent_id) {
                    agent.current_tasks = 0;
                }
                self.schedule_pending().await?;
            }
            
            AgentMessage::DeregisterAgent { agent_id, .. } => {
                self.agents.remove(&agent_id);
            }
            
            AgentMessage::Heartbeat { agent_id: _, .. } => {
                // Update heartbeat timestamp (handled by state store)
            }
            
            _ => {
                // Ignore other message types
            }
        }
        
        Ok(())
    }
    
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> {
        println!("SchedulerAgent started");
        Ok(())
    }
    
    fn status(&self) -> AgentStatus {
        self.definition.status.clone()
    }
}
