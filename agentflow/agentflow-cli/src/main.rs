//! AgentFlow CLI - Command-line interface for AgentFlow

use agentflow_core::{
    Agent, AgentMessage, TaskType, TaskStatus, TaskFilter,
    SystemState,
};
use agentflow_agents::{
    PlannerAgent, SchedulerAgent, NixExecutorAgent, FlakeAnalyzerAgent,
};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// AgentFlow CLI - Manage and monitor the AgentFlow system
#[derive(Parser, Debug)]
#[command(name = "agentflow")]
#[command(author = "Tobias Weiss <weissto@hrz.uni-marburg.de>")]
#[command(version = "0.1.0")]
#[command(about = "Sovereign Agent-Driven CI/CD Platform")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the AgentFlow control plane
    Server {
        /// Configuration file path
        #[arg(short, long, default_value = "agentflow.yaml")]
        config: String,
    },
    
    /// Submit a new task
    Submit {
        /// Task type
        #[arg(short, long)]
        task_type: TaskType,
        
        /// Flake URL (for Nix tasks)
        #[arg(short, long)]
        flake_url: Option<String>,
        
        /// Flake reference (branch, tag, commit)
        #[arg(short, long)]
        flake_ref: Option<String>,
        
        /// Target system
        #[arg(short, long)]
        system: Option<String>,
        
        /// Task priority (0-100)
        #[arg(short, long, default_value = "50")]
        priority: u8,
        
        /// Target outputs
        #[arg(short, long)]
        targets: Option<Vec<String>>,
    },
    
    /// List all tasks
    Tasks {
        /// Filter by status
        #[arg(short, long)]
        status: Option<Vec<TaskStatus>>,
        
        /// Filter by task type
        #[arg(short, long)]
        task_type: Option<Vec<TaskType>>,
        
        /// Limit results
        #[arg(short, long)]
        limit: Option<usize>,
    },
    
    /// List all agents
    Agents,
    
    /// Get task status
    Status {
        /// Task ID
        task_id: String,
    },
    
    /// Analyze a Nix flake
    Analyze {
        /// Flake URL
        flake_url: String,
        
        /// Flake reference (branch, tag, commit)
        #[arg(short, long)]
        ref_: Option<String>,
    },
}

async fn run_agent_system() {
    // Create system state with in-memory stores
    let state = Arc::new(SystemState::new());
    
    // Create message channel
    let (sender, mut receiver) = mpsc::channel(1000);
    
    // Create a simple agent manager
    let mut agents = Vec::<Box<dyn Agent + Send + Sync>>::new();
    
    // Create and start agents
    agents.push(Box::new(PlannerAgent::new(sender.clone(), state.task_store.clone())));
    agents.push(Box::new(SchedulerAgent::new(sender.clone(), state.task_store.clone(), state.agent_store.clone())));
    agents.push(Box::new(NixExecutorAgent::new(sender.clone(), state.task_store.clone(), "x86_64-linux".to_string())));
    agents.push(Box::new(FlakeAnalyzerAgent::new(sender.clone(), state.task_store.clone())));
    
    // Process messages
    while let Some(message) = receiver.recv().await {
        match message {
            AgentMessage::Log { level, message, agent_id, task_id } => {
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                if let Some(task_id_inner) = task_id {
                    println!("[{}] [{}] [{:?}] [{}] {}", timestamp, level, agent_id, task_id_inner, message);
                } else {
                    println!("[{}] [{}] [{:?}] {}", timestamp, level, agent_id, message);
                }
            }
            AgentMessage::TaskResult(result) => {
                println!("Task {} completed with status: {:?}", result.task_id, result.status);
                if let Some(error) = &result.error {
                    println!("  Error: {}", error);
                }
            }
            AgentMessage::TaskFailed { task_id, error } => {
                println!("Task {} failed: {}", task_id, error);
            }
            AgentMessage::TaskScheduled { task_id, agent_id } => {
                println!("Task {} scheduled on agent {}", task_id, agent_id);
            }
            AgentMessage::FlakeAnalysisComplete { task_id: _, flake_url, outputs, .. } => {
                println!("Flake analysis complete for {}: {} outputs", flake_url, outputs.len());
            }
            _ => {
                // Ignore other message types
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Server { config } => {
            println!("Starting AgentFlow control plane...");
            println!("Configuration file: {}", config);
            println!("Press Ctrl+C to stop");
            run_agent_system().await;
        }
        
        Commands::Submit { task_type, flake_url, flake_ref, system, priority, targets } => {
            // Create state store
            let state = Arc::new(SystemState::new());
            let (sender, mut receiver) = mpsc::channel(1000);
            
            // Create minimal agent set
            let _planner = PlannerAgent::new(sender.clone(), state.task_store.clone());
            
            // Generate task ID
            let task_id = Uuid::new_v4().to_string();
            
            // Create task
            let task = agentflow_core::TaskDefinition {
                id: task_id.clone(),
                task_type: task_type.clone(),
                status: agentflow_core::TaskStatus::Pending,
                priority,
                created_at: chrono::Utc::now(),
                flake_url,
                flake_ref,
                system,
                targets,
                ..Default::default()
            };
            
            println!("Submitting task {}: {:?}", task_id, task_type);
            
            // Store task
            state.task_store.create_task(&task).await?;
            
            // Send to planner
            sender.send(AgentMessage::SubmitTask(task)).await?;
            
            println!("Task submitted. Waiting for result...");
            
            // Wait for result
            while let Some(message) = receiver.recv().await {
                match &message {
                    AgentMessage::TaskResult(result) => {
                        if result.task_id == task_id {
                            println!("Task completed: {:?}", result.status);
                            break;
                        }
                    }
                    AgentMessage::TaskFailed { task_id: tid, error } => {
                        if tid == &task_id {
                            println!("Task failed: {}", error);
                            break;
                        }
                    }
                    AgentMessage::TaskScheduled { task_id: tid, agent_id } => {
                        if tid == &task_id {
                            println!("Task scheduled on {}", agent_id);
                        }
                    }
                    _ => {}
                }
            }
        }
        
        Commands::Tasks { status, task_type, limit } => {
            let state = Arc::new(SystemState::new());
            
            let filter = TaskFilter {
                status: status.map(|v| v.into_iter().collect()),
                task_type: task_type.map(|v| v.into_iter().collect()),
                ..Default::default()
            };
            
            let tasks = state.task_store.list_tasks(Some(filter)).await?;
            
            let limit = limit.unwrap_or(tasks.len());
            let tasks = &tasks[..std::cmp::min(limit, tasks.len())];
            
            for task in tasks {
                println!("{:30} {:15?} {:15?} Prior:{} Flake: {:?}",
                    task.id,
                    task.task_type,
                    task.status,
                    task.priority,
                    task.flake_url.as_deref().unwrap_or(""));
            }
            
            println!("\nTotal tasks: {}", tasks.len());
        }
        
        Commands::Agents => {
            unimplemented!("Agents listing not yet implemented");
        }
        
        Commands::Status { task_id } => {
            let state = Arc::new(SystemState::new());
            
            match state.task_store.get_task(&task_id).await? {
                Some(task) => {
                    println!("Task: {}", task.id);
                    println!("  Type: {:?}", task.task_type);
                    println!("  Status: {:?}", task.status);
                    println!("  Priority: {}", task.priority);
                    println!("  Created: {}", task.created_at);
                    if let Some(started) = task.started_at {
                        println!("  Started: {}", started);
                    }
                    if let Some(completed) = task.completed_at {
                        println!("  Completed: {}", completed);
                    }
                    if let Some(flake_url) = &task.flake_url {
                        println!("  Flake URL: {}", flake_url);
                    }
                    if let Some(targets) = &task.targets {
                        println!("  Targets: {:?}", targets);
                    }
                }
                None => {
                    println!("Task not found: {}", task_id);
                }
            }
        }
        
        Commands::Analyze { flake_url, ref_ } => {
            let state = Arc::new(SystemState::new());
            let (sender, mut receiver) = mpsc::channel(1000);
            
            let _planner = PlannerAgent::new(sender.clone(), state.task_store.clone());
            let _analyzer = FlakeAnalyzerAgent::new(sender.clone(), state.task_store.clone());
            
            let task_id = Uuid::new_v4().to_string();
            
            sender.send(AgentMessage::AnalyzeFlake {
                flake_url: flake_url.clone(),
                flake_ref: ref_.clone(),
                task_id: task_id.clone(),
            }).await?;
            
            println!("Analyzing flake: {}#{}", flake_url, ref_.as_deref().unwrap_or("main"));
            println!("Waiting for results...");
            
            while let Some(message) = receiver.recv().await {
                if let AgentMessage::FlakeAnalysisComplete { outputs, .. } = message {
                    println!("\nFlake outputs:");
                    for output in outputs {
                        println!("  {:20} Type: {:15} Drv: {:?}", 
                            output.name, output.output_type, output.drv_path);
                    }
                    return Ok(());
                }
            }
        }
    }
    
    Ok(())
}
