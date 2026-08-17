//! AI Code Reviewer Agent
//!
//! An agent that performs AI-powered code review on Nix flakes and Nix expressions.
//! Uses LLM APIs to analyze code quality, detect anti-patterns, and suggest improvements.

use agentflow_core::{
    Agent, AgentContext, AgentDefinition, AgentMessage, AgentType, TaskDefinition, TaskResult,
    TaskStatus, TaskType, AgentFlowError as Error, Result,
};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// AI Code Reviewer Agent
///
/// Analyzes Nix code for:
/// - Quality and style issues
/// - Anti-patterns
/// - Security vulnerabilities
/// - Performance problems
/// - Best practice violations
pub struct AICodeReviewerAgent {
    /// Agent identifier
    id: String,
    
    /// Agent capabilities
    capabilities: HashSet<String>,
    
    /// LLM configuration
    config: AIReviewerConfig,
    
    /// Message sender
    sender: mpsc::Sender<AgentMessage>,
    
    /// Pending reviews
    pending_reviews: Arc<RwLock<HashMap<String, ReviewState>>>,
    
    /// Review history
    completed_reviews: Arc<RwLock<Vec<ReviewResult>>>,
    
    /// Task store reference
    task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
}

impl AICodeReviewerAgent {
    /// Create a new AI Code Reviewer Agent
    pub fn new(
        id: String,
        config: AIReviewerConfig,
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    ) -> Self {
        let mut capabilities = HashSet::new();
        capabilities.insert("code-review".to_string());
        capabilities.insert("nix-analysis".to_string());
        capabilities.insert("security-scan".to_string());
        capabilities.insert("quality-check".to_string());
        
        Self {
            id: id.clone(),
            capabilities,
            config,
            sender,
            pending_reviews: Arc::new(RwLock::new(HashMap::new())),
            completed_reviews: Arc::new(RwLock::new(Vec::new())),
            task_store,
        }
    }
    
    /// Create agent from definition
    pub fn from_definition(
        definition: &AgentDefinition,
        sender: mpsc::Sender<AgentMessage>,
        task_store: Arc<dyn agentflow_core::agent::TaskStore + Send + Sync>,
    ) -> Result<Self> {
        let config: AIReviewerConfig = if definition.config == serde_json::Value::Null {
            AIReviewerConfig::default()
        } else {
            serde_json::from_value(definition.config.clone()).unwrap_or_else(|_| AIReviewerConfig::default())
        };
        
        Ok(Self::new(definition.id.clone(), config, sender, task_store))
    }
    

    /// Agent type (static)
    fn agent_type_static() -> AgentType {
        AgentType::AICodeReviewer
    }
    
    /// Review code asynchronously
    pub async fn review_code(&self, request: CodeReviewRequest) -> Result<ReviewResult> {
        // Check if we're already reviewing this
        {
            let pending = self.pending_reviews.read().await;
            if pending.contains_key(&request.task_id) {
                return Err(Error::Generic(format!(
                    "Already reviewing task {}", request.task_id
                )));
            }
        }
        
        // Add to pending
        {
            let mut pending = self.pending_reviews.write().await;
            pending.insert(request.task_id.clone(), ReviewState {
                started_at: chrono::Utc::now(),
                status: ReviewStatus::InProgress,
            });
        }
        
        // Perform the review
        let result = self.perform_review(&request).await?;
        
        // Store result
        {
            let mut pending = self.pending_reviews.write().await;
            pending.remove(&request.task_id);
        }
        {
            let mut completed = self.completed_reviews.write().await;
            completed.push(result.clone());
        }
        
        Ok(result)
    }
    
    /// Perform the actual code review using LLM
    async fn perform_review(&self, request: &CodeReviewRequest) -> Result<ReviewResult> {
        // Step 1: Get the code to review
        let code = self.fetch_code(request).await?;
        
        // Step 2: Analyze with LLM
        let analysis = self.analyze_with_llm(request, &code).await?;
        
        // Step 3: Generate review result
        Ok(ReviewResult {
            task_id: request.task_id.clone(),
            flake_url: request.flake_url.clone(),
            flake_ref: request.flake_ref.clone(),
            commit_hash: request.commit_hash.clone(),
            status: ReviewStatus::Completed,
            score: analysis.quality_score,
            findings: analysis.findings.clone(),
            suggestions: analysis.suggestions.clone(),
            reviewed_at: chrono::Utc::now(),
        })
    }
    
    /// Fetch code from various sources
    async fn fetch_code(&self, request: &CodeReviewRequest) -> Result<CodeContent> {
        // Try different sources in order
        
        // 1. If flake URL is provided, fetch from Git
        if let Some(ref url) = request.flake_url {
            if let Ok(content) = self.fetch_from_git(url, &request.flake_ref).await {
                return Ok(content);
            }
        }
        
        // 2. If file paths are provided
        if !request.file_paths.is_empty() {
            return self.fetch_local_files(&request.file_paths).await;
        }
        
        // 3. If raw content is provided
        if !request.raw_content.is_empty() {
            return Ok(CodeContent {
                files: request.raw_content.iter().map(|(path, content)| FileContent {
                    path: path.clone(),
                    content: content.clone(),
                    diff: None,
                }).collect(),
            });
        }
        
        Err(Error::Generic("No code source provided for review".to_string()))
    }
    
    /// Fetch code from git repository
    async fn fetch_from_git(&self, _url: &str, _ref: &Option<String>) -> Result<CodeContent> {
        // TODO: Implement git fetching using git2 or similar
        // For now, return a placeholder
        Err(Error::Generic("Git fetching not yet implemented".to_string()))
    }
    
    /// Fetch local files
    async fn fetch_local_files(&self, paths: &[String]) -> Result<CodeContent> {
        use tokio::fs::File;
        use tokio::io::AsyncReadExt;
        
        let mut files = Vec::new();
        
        for path in paths {
            let mut file = File::open(path).await
                .map_err(|e| Error::Generic(format!("Failed to open {}: {}", path, e)))?;
            
            let mut content = String::new();
            file.read_to_string(&mut content).await
                .map_err(|e| Error::Generic(format!("Failed to read {}: {}", path, e)))?;
            
            files.push(FileContent {
                path: path.clone(),
                content,
                diff: None,
            });
        }
        
        Ok(CodeContent { files })
    }
    
    /// Analyze code with LLM
    async fn analyze_with_llm(&self, request: &CodeReviewRequest, code: &CodeContent) -> Result<LLMAnalysis> {
        // Build the prompt
        let prompt = self.build_review_prompt(request, code);
        
        // Call the LLM API
        let response = self.call_llm(&prompt).await?;
        
        // Parse the response
        self.parse_llm_response(&response)
    }
    
    /// Build the review prompt
    fn build_review_prompt(&self, request: &CodeReviewRequest, code: &CodeContent) -> String {
        let mut prompt = NIX_CODE_REVIEW_PROMPT.to_string();
        
        // Add context
        prompt.push_str(&format!("\n\n## Context\n"));
        if let Some(ref url) = request.flake_url {
            prompt.push_str(&format!("Repository: {}\n", url));
        }
        if let Some(ref ref_) = request.flake_ref {
            prompt.push_str(&format!("Reference: {}\n", ref_));
        }
        if let Some(ref hash) = request.commit_hash {
            prompt.push_str(&format!("Commit: {}\n", hash));
        }
        
        // Add code
        prompt.push_str("\n\n## Code to Review\n\n");
        for file in &code.files {
            prompt.push_str(&format!("### File: {}\n\n{}\n\n", file.path, file.content));
            if let Some(ref diff) = file.diff {
                prompt.push_str(&format!("### Diff:\n\n{}\n\n", diff));
            }
        }
        
        // Add instructions
        prompt.push_str("\n\n## Instructions\n");
        prompt.push_str("Analyze the code and provide a comprehensive review. ");
        prompt.push_str("Focus on Nix-specific best practices, code quality, security, and performance. ");
        prompt.push_str("Respond in JSON format with the following schema:\n");
        prompt.push_str(r#"{
  "quality_score": <0-100>,
  "findings": [
    {
      "severity": "<high|medium|low>",
      "category": "<bug|style|security|performance|anti-pattern>",
      "message": "<description>",
      "file": "<filename>",
      "line": <line_number>,
      "code": "<relevant_code_snippet>"
    }
  ],
  "suggestions": [
    {
      "title": "<title>",
      "description": "<description>",
      "file": "<filename>",
      "replacement": "<suggested_code>"
    }
  ]
}"#);
        
        prompt
    }
    
    /// Call the LLM API
    async fn call_llm(&self, prompt: &str) -> Result<String> {
        match &self.config.provider {
            LLMProvider::OpenAI => self.call_openai(prompt).await,
            LLMProvider::Anthropic => self.call_anthropic(prompt).await,
            LLMProvider::Local => self.call_local_llm(prompt).await,
        }
    }
    
    /// Call OpenAI API
    async fn call_openai(&self, prompt: &str) -> Result<String> {
        use reqwest::Client;
        
        let client = Client::new();
        let api_key = std::env::var("OPENAI_API_KEY")
            .ok()
            .or(self.config.api_key.clone())
            .ok_or_else(|| Error::Generic("OpenAI API key not configured".to_string()))?;
        
        let request_body = serde_json::json!({
            "model": &self.config.model,
            "messages": [
                {
                    "role": "system",
                    "content": "You are an expert Nix code reviewer. Analyze code for quality, security, and best practices."
                },
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "temperature": self.config.temperature,
            "max_tokens": self.config.max_tokens,
        });
        
        let response = client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Error::Generic(format!("OpenAI request failed: {}", e)))?;
        
        if !response.status().is_success() {
            let error: serde_json::Value = response.json().await
                .map_err(|e| Error::Generic(format!("OpenAI error parse failed: {}", e)))?;
            return Err(Error::Generic(format!(
                "OpenAI API error: {}",
                error.get("message").unwrap_or(&error)
            )));
        }
        
        let json: serde_json::Value = response.json().await
            .map_err(|e| Error::Generic(format!("OpenAI response parse failed: {}", e)))?;
        
        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .ok_or(Error::Generic("No content in OpenAI response".to_string()))?
            .to_string();
        
        Ok(content)
    }
    
    /// Call Anthropic API
    async fn call_anthropic(&self, prompt: &str) -> Result<String> {
        use reqwest::Client;
        
        let client = Client::new();
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .or(self.config.api_key.clone())
            .ok_or_else(|| Error::Generic("Anthropic API key not configured".to_string()))?;
        
        let request_body = serde_json::json!({
            "model": &self.config.model,
            "prompt": format!("\n\nHuman: {}\n\nAssistant:", prompt),
            "max_tokens_to_sample": self.config.max_tokens,
            "temperature": self.config.temperature,
        });
        
        let response = client
            .post("https://api.anthropic.com/v1/complete")
            .header("x-api-key", api_key)
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Error::Generic(format!("Anthropic request failed: {}", e)))?;
        
        if !response.status().is_success() {
            let error: serde_json::Value = response.json().await
                .map_err(|e| Error::Generic(format!("Anthropic error parse failed: {}", e)))?;
            return Err(Error::Generic(format!(
                "Anthropic API error: {:?}",
                error
            )));
        }
        
        let json: serde_json::Value = response.json().await
            .map_err(|e| Error::Generic(format!("Anthropic response parse failed: {}", e)))?;
        
        let content = json["completion"]
            .as_str()
            .ok_or(Error::Generic("No content in Anthropic response".to_string()))?
            .to_string();
        
        Ok(content)
    }
    
    /// Call local LLM
    async fn call_local_llm(&self, prompt: &str) -> Result<String> {
        // Try different local providers
        // 1. Try Ollama
        if let Ok(response) = self.call_ollama(prompt).await {
            return Ok(response);
        }
        
        // 2. Try any other local endpoint
        if let Some(ref endpoint) = self.config.local_endpoint {
            return self.call_generic_local(endpoint, prompt).await;
        }
        
        Err(Error::Generic("No local LLM endpoint configured".to_string()))
    }
    
    /// Call Ollama
    async fn call_ollama(&self, prompt: &str) -> Result<String> {
        use reqwest::Client;
        
        let client = Client::new();
        let endpoint = self.config.local_endpoint
            .as_deref()
            .unwrap_or("http://localhost:11434/api/generate");
        
        let request_body = serde_json::json!({
            "model": &self.config.model,
            "prompt": prompt,
            "stream": false,
            "options": {
                "temperature": self.config.temperature,
            }
        });
        
        let response = client
            .post(endpoint)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Error::Generic(format!("Ollama request failed: {}", e)))?;
        
        if !response.status().is_success() {
            return Err(Error::Generic(format!(
                "Ollama API error: {}",
                response.status()
            )));
        }
        
        let json: serde_json::Value = response.json().await
            .map_err(|e| Error::Generic(format!("Ollama response parse failed: {}", e)))?;
        
        let content = json["response"]
            .as_str()
            .ok_or(Error::Generic("No content in Ollama response".to_string()))?
            .to_string();
        
        Ok(content)
    }
    
    /// Call generic local LLM endpoint
    async fn call_generic_local(&self, endpoint: &str, prompt: &str) -> Result<String> {
        use reqwest::Client;
        
        let client = Client::new();
        let request_body = serde_json::json!({
            "prompt": prompt,
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
        });
        
        let response = client
            .post(endpoint)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| Error::Generic(format!("Local LLM request failed: {}", e)))?;
        
        if !response.status().is_success() {
            return Err(Error::Generic(format!(
                "Local LLM API error: {}",
                response.status()
            )));
        }
        
        response.text().await
            .map_err(|e| Error::Generic(format!("Local LLM response failed: {}", e)))
    }
    
    /// Parse LLM response into structured analysis
    fn parse_llm_response(&self, response: &str) -> Result<LLMAnalysis> {
        // Try to parse as JSON first
        if response.trim().starts_with('{') {
            if let Ok(analysis) = serde_json::from_str::<LLMAnalysis>(response) {
                return Ok(analysis);
            }
        }
        
        // Fall back to parsing text response
        // This is a simple parser - should be enhanced
        let mut analysis = LLMAnalysis::default();
        
        // Extract score (look for patterns like "Score: 85" or "85/100")
        if let Ok(re) = regex::Regex::new(r"(\d{1,3})/100") {
            if let Some(cap) = re.captures(response) {
                if let Ok(score) = cap[1].parse::<u8>() {
                    analysis.quality_score = score;
                }
            }
        }
        
        // Add the full response as a single finding
        analysis.findings.push(Finding {
            severity: "info".to_string(),
            category: "general".to_string(),
            message: "LLM analysis".to_string(),
            file: None,
            line: None,
            code: None,
        });
        
        analysis.suggestions.push(Suggestion {
            title: "LLM Response".to_string(),
            description: response.to_string(),
            file: None,
            replacement: None,
        });
        
        Ok(analysis)
    }
    
    /// Review a task and send results
    pub async fn review_task(&self, task: &TaskDefinition) -> Result<()> {
        let mut focus_areas = HashSet::new();
        focus_areas.insert("quality".to_string());
        focus_areas.insert("security".to_string());
        focus_areas.insert("performance".to_string());
        
        let request = CodeReviewRequest {
            task_id: task.id.clone(),
            flake_url: task.flake_url.clone(),
            flake_ref: task.flake_ref.clone(),
            commit_hash: None,
            file_paths: vec![],
            raw_content: HashMap::new(),
            focus_areas,
        };
        
        let task_id = request.task_id.clone();
        let result = self.review_code(request).await?;
        
        // Convert our ReviewResult to AIReview
        let findings_count = result.findings.len();
        let suggestions_count = result.suggestions.len();
        
        let ai_review = agentflow_core::message::AIReview {
            approved: result.score >= 70, // Approve if score >= 70%
            score: Some(result.score as f32 / 100.0),
            findings: result.findings.into_iter().map(|f| agentflow_core::message::AIFinding {
                severity: f.severity,
                category: f.category,
                description: f.message,
                location: f.file.map(|file| agentflow_core::message::AILocation {
                    file,
                    line: f.line.map(|l| l as u32),
                    column: None,
                    code: f.code.clone(),
                }),
                fix_suggestion: f.code,
            }).collect(),
            suggestions: result.suggestions.into_iter().map(|s| s.description).collect(),
            summary: Some(format!(
                "Code review completed with score {}/100. {} findings, {} suggestions.",
                result.score,
                findings_count,
                suggestions_count
            )),
        };
        
        // Send result as CodeReviewComplete message
        let message = AgentMessage::CodeReviewComplete {
            task_id,
            review: ai_review,
        };
        self.sender.send(message).await
            .map_err(|e| Error::ChannelSend(e))?;
        
        // Update task with review results in metadata
        let metadata = {
            let mut m = HashMap::new();
            m.insert("review_score".to_string(), result.score.to_string());
            m.insert("review_findings".to_string(), findings_count.to_string());
            m.insert("review_suggestions".to_string(), suggestions_count.to_string());
            m
        };
        
        let update = agentflow_core::agent::TaskUpdate {
            status: Some(TaskStatus::Succeeded),
            completed_at: Some(chrono::Utc::now()),
            metadata: Some(metadata),
            ..Default::default()
        };
        
        self.task_store.update_task(&task.id, update).await?;
        
        Ok(())
    }
}

/// Configuration for AI Code Reviewer
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AIReviewerConfig {
    /// LLM provider
    pub provider: LLMProvider,
    
    /// Model to use
    pub model: String,
    
    /// API key (optional, can use env var)
    pub api_key: Option<String>,
    
    /// Local LLM endpoint (for Ollama, etc.)
    pub local_endpoint: Option<String>,
    
    /// Temperature (0-1)
    pub temperature: f32,
    
    /// Maximum tokens
    pub max_tokens: u32,
    
    /// Timeout in seconds
    pub timeout: u64,
    
    /// Retry count
    pub max_retries: u32,
    
    /// Custom prompts
    pub prompts: Option<ReviewPrompts>,
}

impl Default for AIReviewerConfig {
    fn default() -> Self {
        Self {
            provider: LLMProvider::OpenAI,
            model: "gpt-4".to_string(),
            api_key: None,
            local_endpoint: None,
            temperature: 0.3,
            max_tokens: 4096,
            timeout: 60,
            max_retries: 3,
            prompts: Some(ReviewPrompts::default()),
        }
    }
}

/// LLM Provider
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, strum::EnumString)]
pub enum LLMProvider {
    /// OpenAI (GPT-3.5, GPT-4)
    OpenAI,
    /// Anthropic (Claude)
    Anthropic,
    /// Local LLM (Ollama, LM Studio, etc.)
    Local,
}

/// Custom review prompts
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewPrompts {
    /// System prompt
    pub system: String,
    /// Quality prompt
    pub quality: String,
    /// Security prompt
    pub security: String,
    /// Performance prompt
    pub performance: String,
    /// Style prompt
    pub style: String,
}

impl Default for ReviewPrompts {
    fn default() -> Self {
        Self {
            system: "You are an expert Nix code reviewer. Analyze code thoroughly.".to_string(),
            quality: "Check for code quality issues, readability, and maintainability.".to_string(),
            security: "Identify potential security vulnerabilities and risks.".to_string(),
            performance: "Analyze performance characteristics and suggest optimizations.".to_string(),
            style: "Check for adherence to Nix style guidelines and idioms.".to_string(),
        }
    }
}

/// Default Nix code review prompt
pub const NIX_CODE_REVIEW_PROMPT: &str = r#"You are an expert Nix code reviewer with deep knowledge of:
- Nix language syntax and semantics
- Nixpkgs conventions and patterns
- Flake-based development
- Functional package management
- Reproducible builds
- Security best practices for Nix

Your task is to comprehensively analyze Nix code for:
1. **Code Quality**: Readability, maintainability, idiomatic Nix
2. **Security**: Vulnerabilities, unsafe practices, dependency risks
3. **Performance**: Inefficient computations, slow derivations
4. **Correctness**: Logical errors, incorrect assumptions
5. **Best Practices**: Following Nix community conventions"#;

/// LLM Analysis result
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LLMAnalysis {
    pub quality_score: u8,
    pub findings: Vec<Finding>,
    pub suggestions: Vec<Suggestion>,
}

impl Default for LLMAnalysis {
    fn default() -> Self {
        Self {
            quality_score: 50,
            findings: vec![],
            suggestions: vec![],
        }
    }
}

/// Code review finding
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    pub severity: String,
    pub category: String,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub code: Option<String>,
}

/// Code review suggestion
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Suggestion {
    pub title: String,
    pub description: String,
    pub file: Option<String>,
    pub replacement: Option<String>,
}

/// Code review request
#[derive(Debug, Clone)]
pub struct CodeReviewRequest {
    pub task_id: String,
    pub flake_url: Option<String>,
    pub flake_ref: Option<String>,
    pub commit_hash: Option<String>,
    pub file_paths: Vec<String>,
    pub raw_content: HashMap<String, String>,
    pub focus_areas: HashSet<String>,
}

/// Code review result
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReviewResult {
    pub task_id: String,
    pub flake_url: Option<String>,
    pub flake_ref: Option<String>,
    pub commit_hash: Option<String>,
    pub status: ReviewStatus,
    pub score: u8,
    pub findings: Vec<Finding>,
    pub suggestions: Vec<Suggestion>,
    pub reviewed_at: chrono::DateTime<chrono::Utc>,
}

/// Code review result for task
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeReviewResult {
    pub score: u8,
    pub findings_count: usize,
    pub suggestions_count: usize,
}

/// Review status
#[derive(Debug, Clone, strum::EnumString, serde::Serialize, serde::Deserialize)]
pub enum ReviewStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Review state
#[derive(Debug, Clone)]
pub struct ReviewState {
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub status: ReviewStatus,
}

/// Code content
#[derive(Debug, Clone)]
pub struct CodeContent {
    pub files: Vec<FileContent>,
}

/// File content with optional diff
#[derive(Debug, Clone)]
pub struct FileContent {
    pub path: String,
    pub content: String,
    pub diff: Option<String>,
}

/// Agent implementation
#[async_trait]
impl Agent for AICodeReviewerAgent {
    fn name(&self) -> &str {
        &self.id
    }
    
    fn agent_type(&self) -> AgentType {
        Self::agent_type_static()
    }
    
    fn capabilities(&self) -> &HashSet<String> {
        &self.capabilities
    }
    
    async fn handle_message(&mut self, message: AgentMessage, ctx: &AgentContext) -> Result<()> {
        match message {
            AgentMessage::ExecuteTask(task) => {
                if task.task_type == TaskType::AICodeReview {
                    self.review_task(&task).await?;
                    Ok(())
                } else {
                    // Not our task type
                    Ok(())
                }
            }
            AgentMessage::RequestCodeReview { repo_url, branch, changes, handbook: _, task_id } => {
                // Convert RequestCodeReview to our CodeReviewRequest
                let our_request = CodeReviewRequest {
                    task_id: task_id.clone(),
                    flake_url: Some(repo_url.clone()),
                    flake_ref: Some(branch.clone()),
                    commit_hash: None,
                    file_paths: vec![],
                    raw_content: {
                        let mut m = HashMap::new();
                        m.insert("default.nix".to_string(), changes.clone());
                        m
                    },
                    focus_areas: {
                        let mut fs = HashSet::new();
                        fs.insert("quality".to_string());
                        fs.insert("security".to_string());
                        fs.insert("correctness".to_string());
                        fs
                    },
                };
                let result = self.review_code(our_request).await?;
                let message = AgentMessage::CodeReviewComplete {
                    task_id: task_id.clone(),
                    review: agentflow_core::message::AIReview {
                        approved: result.score >= 70,
                        score: Some(result.score as f32 / 100.0),
                        findings: result.findings.into_iter().map(|f| agentflow_core::message::AIFinding {
                            severity: f.severity,
                            category: f.category,
                            description: f.message,
                            location: f.file.map(|file| agentflow_core::message::AILocation {
                                file,
                                line: f.line.map(|l| l as u32),
                                column: None,
                                code: f.code.clone(),
                            }),
                            fix_suggestion: f.code,
                        }).collect(),
                        suggestions: result.suggestions.into_iter().map(|s| s.description).collect(),
                        summary: Some(format!(
                            "Code review completed with score {}/100",
                            result.score
                        )),
                    },
                };
                ctx.sender.send(message).await
                    .map_err(|e| Error::ChannelSend(e))?;
                Ok(())
            }
            _ => {
                // Ignore other message types
                Ok(())
            }
        }
    }
    
    async fn on_start(&mut self, _ctx: &AgentContext) -> Result<()> {
        tracing::info!("AICodeReviewerAgent {} started", self.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_prompt_content() {
        // NIX_CODE_REVIEW_PROMPT should be descriptive
        assert!(NIX_CODE_REVIEW_PROMPT.len() > 100);
        assert!(NIX_CODE_REVIEW_PROMPT.contains("Nix"));
        assert!(NIX_CODE_REVIEW_PROMPT.contains("code"));
    }
}
