//! Nix Executor Agent - Executes Nix evaluations and builds

use agentflow_core::{
    Agent, AgentContext, AgentDefinition, AgentStatus, AgentType, AgentMessage,
    Result, TaskDefinition, TaskType, TaskStatus, TaskResult,
};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::mpsc;
use std::time::Instant;

/// Nix Executor Agent - Executes Nix evaluations and builds
/// 
/// This agent is inspired by argunix's execution engine.
/// It runs `nix eval` and `nix build` commands.
pub struct NixExecutorAgent {
    /// Agent definition
    definition: AgentDefinition,
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    /// System platform (e.g., x86_64-linux)
    system: String,
    /// Nix command path
    nix_command: String,
    /// Maximum concurrent builds
    max_concurrent: u32,
    /// Current active tasks
    current_tasks: u32,
}

impl NixExecutorAgent {
    /// Create a new NixExecutorAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
        system: String,
    ) -> Self {
        Self {
            definition: AgentDefinition {
                id: format!("nix-executor-{}", system),
                name: format!("Nix Executor ({})", system),
                agent_type: AgentType::NixExecutor,
                status: AgentStatus::Ready,
                capabilities: HashSet::from([
                    "nix-eval".to_string(),
                    "nix-build".to_string(),
                    "nix-check".to_string(),
                    format!("{}", system),
                ]),
                ..Default::default()
            },
            sender,
            task_store,
            system,
            nix_command: "nix".to_string(),
            max_concurrent: 4,
            current_tasks: 0,
        }
    }
    
    /// Check if we can execute this task
    fn can_execute(&self, task: &TaskDefinition) -> bool {
        match task.task_type {
            TaskType::NixEval | TaskType::NixBuild | TaskType::NixCheck => {
                // Check system match
                if let Some(task_system) = &task.system {
                    task_system == &self.system
                } else {
                    true // No system specified, assume compatible
                }
            }
            TaskType::NixDevShell | TaskType::NixBundle => {
                if let Some(task_system) = &task.system {
                    task_system == &self.system
                } else {
                    true
                }
            }
            _ => false,
        }
    }
    
    /// Execute a Nix eval command
    async fn execute_nix_eval(&self, task: &TaskDefinition) -> Result<TaskResult> {
        let flake_url = task.flake_url.as_deref().unwrap_or("");
        let flake_ref = task.flake_ref.as_deref().unwrap_or("main");
        
        let start = Instant::now();
        
        // Build nix eval command
        let mut cmd = Command::new(&self.nix_command);
        cmd.arg("eval");
        cmd.arg(format!("--flake={}#{}", flake_url, flake_ref));
        
        if let Some(targets) = &task.targets {
            for target in targets {
                cmd.arg(target);
            }
        } else {
            // Default: evaluate all outputs
            cmd.arg("packages.*.default");
            cmd.arg("checks.*.default");
            cmd.arg("devShells.*.default");
        }
        
        cmd.arg("--json");
        
        let output = cmd.output()?;
        let duration = start.elapsed().as_secs_f64();
        
        let status = if output.status.success() {
            TaskStatus::Succeeded
        } else {
            TaskStatus::Failed
        };
        
        let result = TaskResult {
            task_id: task.id.clone(),
            status,
            output: Some(String::from_utf8_lossy(&output.stdout).to_string()),
            artifacts: None,
            exit_code: Some(output.status.code().unwrap_or(-1)),
            error: if !output.status.success() {
                Some(String::from_utf8_lossy(&output.stderr).to_string())
            } else {
                None
            },
            duration_seconds: Some(duration),
            started_at: Some(chrono::Utc::now()),
            completed_at: Some(chrono::Utc::now()),
            metadata: HashMap::new(),
        };
        
        Ok(result)
    }
    
    /// Execute a Nix build command
    async fn execute_nix_build(&self, task: &TaskDefinition) -> Result<TaskResult> {
        let flake_url = task.flake_url.as_deref().unwrap_or("");
        let flake_ref = task.flake_ref.as_deref().unwrap_or("main");
        
        let start = Instant::now();
        
        // Build nix build command
        let mut cmd = Command::new(&self.nix_command);
        cmd.arg("build");
        cmd.arg(format!("--flake={}#{}", flake_url, flake_ref));
        
        if let Some(targets) = &task.targets {
            for target in targets {
                cmd.arg(target);
            }
        }
        
        cmd.arg("--json");
        
        let output = cmd.output()?;
        let duration = start.elapsed().as_secs_f64();
        
        let status = if output.status.success() {
            TaskStatus::Succeeded
        } else {
            TaskStatus::Failed
        };
        
        let result = TaskResult {
            task_id: task.id.clone(),
            status,
            output: Some(String::from_utf8_lossy(&output.stdout).to_string()),
            artifacts: None,
            exit_code: Some(output.status.code().unwrap_or(-1)),
            error: if !output.status.success() {
                Some(String::from_utf8_lossy(&output.stderr).to_string())
            } else {
                None
            },
            duration_seconds: Some(duration),
            started_at: Some(chrono::Utc::now()),
            completed_at: Some(chrono::Utc::now()),
            metadata: HashMap::new(),
        };
        
        Ok(result)
    }
    
    /// Execute a task
    async fn execute_task(&mut self, task: TaskDefinition) -> Result<()> {
        // Check if we can execute
        if !self.can_execute(&task) {
            return Err(agentflow_core::AgentFlowError::Generic(
                format!("Cannot execute task {} on this executor", task.id)
            ));
        }
        
        // Update task status
        let update = agentflow_core::agent::TaskUpdate {
            status: Some(TaskStatus::Running),
            started_at: Some(chrono::Utc::now()),
            ..Default::default()
        };
        self.task_store.update_task(&task.id, update).await?;
        
        // Increment task count
        self.current_tasks += 1;
        
        let result = match task.task_type {
            TaskType::NixEval => self.execute_nix_eval(&task).await?,
            TaskType::NixBuild => self.execute_nix_build(&task).await?,
            TaskType::NixCheck => self.execute_nix_eval(&task).await?, // Same as eval
            TaskType::NixDevShell => {
                // Create devShell
                self.execute_nix_eval(&task).await?
            }
            TaskType::NixBundle => {
                // Bundle the nix app
                self.execute_nix_build(&task).await?
            }
            _ => {
                return Err(agentflow_core::AgentFlowError::Generic(
                    format!("Unsupported task type: {:?}", task.task_type)
                ));
            }
        };
        
        // Update task status
        let update = agentflow_core::agent::TaskUpdate {
            status: Some(result.status.clone()),
            completed_at: result.completed_at,
            ..Default::default()
        };
        self.task_store.update_task(&task.id, update).await?;
        
        // Decrement task count
        self.current_tasks -= 1;
        
        // Send result
        self.sender.send(AgentMessage::TaskResult(result)).await?;
        
        Ok(())
    }
}

#[async_trait::async_trait]
impl Agent for NixExecutorAgent {
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
            AgentMessage::ExecuteTask(task) => {
                self.execute_task(task).await?;
            }
            
            AgentMessage::EvaluateFlake { flake_url, flake_ref, system, targets, task_id } => {
                let task = TaskDefinition {
                    id: task_id.clone(),
                    task_type: TaskType::NixEval,
                    status: TaskStatus::Running,
                    priority: 80,
                    created_at: chrono::Utc::now(),
                    flake_url: Some(flake_url),
                    flake_ref: Some(flake_ref),
                    system: Some(system),
                    targets: Some(targets),
                    ..Default::default()
                };
                self.execute_task(task).await?;
            }
            
            AgentMessage::BuildDrv { drv_path, task_id } => {
                let task = TaskDefinition {
                    id: task_id.clone(),
                    task_type: TaskType::NixBuild,
                    status: TaskStatus::Running,
                    priority: 60,
                    created_at: chrono::Utc::now(),
                    drv_path: Some(drv_path),
                    ..Default::default()
                };
                self.execute_task(task).await?;
            }
            
            _ => {
                // Ignore other message types
            }
        }
        
        Ok(())
    }
    
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> {
        println!("NixExecutorAgent started for system {}", self.system);
        Ok(())
    }
    
    fn status(&self) -> AgentStatus {
        self.definition.status.clone()
    }
}
