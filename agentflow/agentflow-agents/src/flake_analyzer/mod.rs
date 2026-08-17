//! Flake Analyzer Agent - Analyzes Nix flakes to discover outputs

use agentflow_core::{
    Agent, AgentContext, AgentDefinition, AgentStatus, AgentType, AgentMessage,
    Result,
};
use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::mpsc;
use serde_json::Value;

/// Flake output discovered during analysis
#[derive(Debug, Clone)]
pub struct FlakeOutput {
    pub name: String,
    pub output_type: String,
    pub system: Option<String>,
    pub drv_path: Option<String>,
    pub description: Option<String>,
}

/// Flake Analyzer Agent - Analyzes Nix flakes (inspired by argunix)
pub struct FlakeAnalyzerAgent {
    /// Agent definition
    definition: AgentDefinition,
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    /// Task store
    task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    /// Nix command path
    nix_command: String,
}

impl FlakeAnalyzerAgent {
    /// Create a new FlakeAnalyzerAgent
    pub fn new(
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    ) -> Self {
        Self {
            definition: AgentDefinition {
                id: "flake-analyzer-1".to_string(),
                name: "Flake Analyzer Agent".to_string(),
                agent_type: AgentType::FlakeAnalyzer,
                status: AgentStatus::Ready,
                capabilities: HashSet::from([
                    "flake-analysis".to_string(),
                    "flake-discovery".to_string(),
                ]),
                ..Default::default()
            },
            sender,
            task_store,
            nix_command: "nix".to_string(),
        }
    }
    
    /// Analyze a flake to discover its outputs
    async fn analyze_flake(
        &self,
        flake_url: &str,
        flake_ref: Option<&str>,
    ) -> Result<Vec<FlakeOutput>> {
        let ref_str = flake_ref.unwrap_or("main");
        
        // Use nix flake metadata to discover outputs
        let mut cmd = Command::new(&self.nix_command);
        cmd.arg("flake");
        cmd.arg("metadata");
        cmd.arg(format!("{}#{}", flake_url, ref_str));
        cmd.arg("--json");
        
        let output = cmd.output()?;
        
        if !output.status.success() {
            return Err(agentflow_core::AgentFlowError::Generic(
                format!("Failed to get flake metadata: {}", String::from_utf8_lossy(&output.stderr))
            ));
        }
        
        let metadata: Value = serde_json::from_slice(&output.stdout)?;
        let mut outputs = Vec::new();
        
        // Extract packages
        if let Some(packages) = metadata.get("packages").and_then(Value::as_object) {
            for (name, package) in packages {
                let output = FlakeOutput {
                    name: name.clone(),
                    output_type: "package".to_string(),
                    system: None,
                    drv_path: package.get("drvPath").and_then(Value::as_str).map(String::from),
                    description: package.get("description").and_then(Value::as_str).map(String::from),
                };
                outputs.push(output);
            }
        }
        
        // Extract checks
        if let Some(checks) = metadata.get("checks").and_then(Value::as_object) {
            for (name, check) in checks {
                let output = FlakeOutput {
                    name: name.clone(),
                    output_type: "check".to_string(),
                    system: None,
                    drv_path: check.get("drvPath").and_then(Value::as_str).map(String::from),
                    description: check.get("description").and_then(Value::as_str).map(String::from),
                };
                outputs.push(output);
            }
        }
        
        // Extract devShells
        if let Some(dev_shells) = metadata.get("devShells").and_then(Value::as_object) {
            for (name, shell) in dev_shells {
                let output = FlakeOutput {
                    name: name.clone(),
                    output_type: "devShell".to_string(),
                    system: None,
                    drv_path: shell.get("drvPath").and_then(Value::as_str).map(String::from),
                    description: shell.get("description").and_then(Value::as_str).map(String::from),
                };
                outputs.push(output);
            }
        }
        
        // Extract nixosConfigurations
        if let Some(configs) = metadata.get("nixosConfigurations").and_then(Value::as_object) {
            for (name, _config) in configs {
                let output = FlakeOutput {
                    name: name.clone(),
                    output_type: "nixosConfiguration".to_string(),
                    system: None,
                    drv_path: None,
                    description: None,
                };
                outputs.push(output);
            }
        }
        
        // Extract overlays, modules, etc.
        if let Some(overlays) = metadata.get("overlays").and_then(Value::as_object) {
            for (name, _overlay) in overlays {
                let output = FlakeOutput {
                    name: name.clone(),
                    output_type: "overlay".to_string(),
                    system: None,
                    drv_path: None,
                    description: None,
                };
                outputs.push(output);
            }
        }
        
        if let Some(modules) = metadata.get("modules").and_then(Value::as_object) {
            for (name, _module) in modules {
                let output = FlakeOutput {
                    name: name.clone(),
                    output_type: "module".to_string(),
                    system: None,
                    drv_path: None,
                    description: None,
                };
                outputs.push(output);
            }
        }
        
        Ok(outputs)
    }
    
    /// Determine which outputs can be built for a specific system
    async fn get_outputs_for_system(
        &self,
        flake_url: &str,
        flake_ref: Option<&str>,
        _system: &str,
    ) -> Result<Vec<String>> {
        let outputs = self.analyze_flake(flake_url, flake_ref).await?;
        
        // In a real implementation, we'd use nix-eval-jobs to determine
        // which outputs are available for the system.
        // For now, return all outputs.
        
        Ok(outputs.into_iter().map(|o| o.name).collect())
    }
}

#[async_trait::async_trait]
impl Agent for FlakeAnalyzerAgent {
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
            AgentMessage::AnalyzeFlake { flake_url, flake_ref, task_id } => {
                let outputs = self.analyze_flake(&flake_url, flake_ref.as_deref()).await?;
                
                // Convert to NixOutput format
                let nix_outputs: Vec<agentflow_core::message::NixOutput> = outputs
                    .into_iter()
                    .map(|o| agentflow_core::message::NixOutput {
                        name: o.name,
                        output_type: o.output_type,
                        system: o.system,
                        drv_path: o.drv_path,
                        description: o.description,
                    })
                    .collect();
                
                let message = AgentMessage::FlakeAnalysisComplete {
                    task_id,
                    flake_url,
                    outputs: nix_outputs,
                    dependencies: vec![], // Would be populated in real implementation
                };
                
                self.sender.send(message).await.map_err(|e| {
                    agentflow_core::AgentFlowError::Generic(format!("Failed to send message: {}", e))
                })?;
            }
            
            AgentMessage::EvaluateFlake { flake_url, flake_ref, system, .. } => {
                let outputs = self.get_outputs_for_system(&flake_url, Some(&flake_ref), &system).await?;
                
                println!("Flake {}#{} has {} outputs for system {}", 
                    flake_url, flake_ref, outputs.len(), system);
                
                for output in outputs {
                    println!("  - {}", output);
                }
            }
            
            _ => {
                // Ignore other message types
            }
        }
        
        Ok(())
    }
    
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> {
        println!("FlakeAnalyzerAgent started");
        Ok(())
    }
    
    fn status(&self) -> AgentStatus {
        self.definition.status.clone()
    }
}
