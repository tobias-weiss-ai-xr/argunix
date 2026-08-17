//! AgentFlow Server - HTTP API Gateway

use agentflow_core::{
    AgentMessage, AgentType, TaskDefinition, TaskFilter, TaskStatus, TaskType,
    TaskResult, NixOutput, SystemState,
    agent::TaskUpdate,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, patch, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

mod config;
mod error;
mod state;

use config::ServerConfig;
use error::{ApiError, Result};
use state::AppState;

/// Main entry point for the AgentFlow server
#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration
    let config = ServerConfig::from_env()?;
    
    // Create system state
    let system_state = Arc::new(SystemState::new());
    
    // Create message channel for internal communication
    let (sender, _receiver) = mpsc::channel(10000);
    
    // Create application state
    let app_state = AppState::new(sender, system_state, config.clone());
    
    // Build router
    let app = build_router(app_state);
    
    // Start server
    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    
    println!("AgentFlow server starting on {}", config.bind_address);
    println!("API documentation: http://{}/api/v1/docs", config.bind_address);
    println!("Health check: http://{}/api/v1/health", config.bind_address);
    
    axum::serve(
        listener,
        app.into_make_service(),
    )
    .await
    .map_err(|e| ApiError::InternalServerError(e.to_string()))?;
    
    Ok(())
}

/// Build the Axum router with all API routes
fn build_router(app_state: Arc<AppState>) -> Router {
    // Combine all routes
    let api_router = Router::new()
        // Health and system
        .route("/health", get(health_check))
        .route("/status", get(system_status))
        .route("/metrics", get(prometheus_metrics))
        
        // Tasks
        .route("/tasks", get(list_tasks))
        .route("/tasks", post(create_task))
        .route("/tasks/:id", get(get_task))
        .route("/tasks/:id", patch(update_task))
        .route("/tasks/:id", delete(delete_task))
        .route("/tasks/:id/cancel", post(cancel_task))
        
        // Agents
        .route("/agents", get(list_agents))
        .route("/agents/:id", get(get_agent))
        
        // Flakes
        .route("/flakes/analyze", post(analyze_flake))
        .route("/flakes/:url/outputs", get(get_flake_outputs))
        
        // Webhooks
        .route("/webhooks/github", post(handle_github_webhook))
        .route("/webhooks/gitlab", post(handle_gitlab_webhook))
        .route("/webhooks/forgejo", post(handle_forgejo_webhook))
        
        // Docs
        .route("/docs", get(api_docs))
        .route("/docs.json", get(api_docs_json))
        
        .with_state(app_state.clone());
    
    // Mount under /api/v1
    Router::new()
        .nest("/api/v1", api_router)
        .fallback(handler_404)
}

// ==================== Health & System ====================

/// Health check endpoint
async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp: chrono::Utc::now(),
    })
}

/// System status endpoint
async fn system_status(
    state: State<Arc<AppState>>,
) -> Result<Json<SystemStatusResponse>> {
    let task_count = state.task_store.list_tasks(None).await?.len();
    let agent_count = state.agent_store.list_agents().await?.len();
    let uptime = state.uptime();
    
    Ok(Json(SystemStatusResponse {
        tasks_total: task_count,
        tasks_pending: 0,
        tasks_running: 0,
        agents_total: agent_count,
        agents_active: 0,
        uptime,
    }))
}

/// Prometheus metrics endpoint
async fn prometheus_metrics() -> &'static str {
    "# AgentFlow Metrics\n# HELP agentflow_tasks_total Total number of tasks\n# TYPE agentflow_tasks_total counter\nagentflow_tasks_total 0\n"
}

// ==================== Tasks ====================

/// List tasks with optional filters
async fn list_tasks(
    state: State<Arc<AppState>>,
    query: Query<ListTasksQuery>,
) -> Result<Json<TaskListResponse>> {
    let filter = TaskFilter {
        status: query.status.clone(),
        task_type: query.task_type.clone(),
        priority_min: query.priority_min,
        priority_max: query.priority_max,
        limit: query.limit,
        ..Default::default()
    };
    
    let tasks = state.task_store.list_tasks(Some(filter)).await?;
    
    let total = if query.limit.is_some() {
        let all_tasks = state.task_store.list_tasks(None).await?;
        all_tasks.len()
    } else {
        tasks.len()
    };
    
    Ok(Json(TaskListResponse {
        tasks,
        total,
        limit: query.limit,
        offset: 0,
    }))
}

/// Create a new task
async fn create_task(
    state: State<Arc<AppState>>,
    Json(request): Json<CreateTaskRequest>,
) -> Result<impl IntoResponse> {
    let task_id = Uuid::new_v4().to_string();
    
    let task = TaskDefinition {
        id: task_id.clone(),
        task_type: request.task_type,
        status: TaskStatus::Pending,
        priority: request.priority.unwrap_or(50),
        created_at: chrono::Utc::now(),
        flake_url: request.flake_url,
        flake_ref: request.flake_ref,
        system: request.system,
        targets: request.targets,
        depends_on: request.depends_on,
        ..Default::default()
    };
    
    // Store task
    state.task_store.create_task(&task).await?;
    
    // Send to message bus
    state.sender.send(AgentMessage::SubmitTask(task.clone())).await
        .map_err(|e| ApiError::InternalServerError(e.to_string()))?;
    
    Ok((StatusCode::CREATED, Json(TaskResponse { task })))
}

/// Get task details
async fn get_task(
    state: State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<TaskResponse>> {
    let task = state.task_store.get_task(&task_id).await?;
    
    match task {
        Some(task) => Ok(Json(TaskResponse { task })),
        None => Err(ApiError::NotFound("Task not found".to_string())),
    }
}

/// Update task
async fn update_task(
    state: State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Json(request): Json<UpdateTaskRequest>,
) -> Result<Json<TaskResponse>> {
    let update = TaskUpdate {
        status: request.status,
        priority: request.priority,
        started_at: request.started_at,
        completed_at: request.completed_at,
        ..Default::default()
    };
    
    state.task_store.update_task(&task_id, update).await?;
    
    let task = state.task_store.get_task(&task_id).await?;
    
    match task {
        Some(task) => Ok(Json(TaskResponse { task })),
        None => Err(ApiError::NotFound("Task not found".to_string())),
    }
}

/// Delete task
async fn delete_task(
    state: State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<DeleteResponse>> {
    state.task_store.delete_task(&task_id).await?;
    
    Ok(Json(DeleteResponse {
        deleted: true,
        id: task_id,
    }))
}

/// Cancel task
async fn cancel_task(
    state: State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<TaskResponse>> {
    let update = TaskUpdate {
        status: Some(TaskStatus::Cancelled),
        ..Default::default()
    };
    
    state.task_store.update_task(&task_id, update).await?;
    
    // Send cancellation message
    state.sender.send(AgentMessage::CancelTask { task_id: task_id.clone(), reason: "Cancelled via API".to_string() }).await
        .map_err(|e| ApiError::InternalServerError(e.to_string()))?;
    
    let task = state.task_store.get_task(&task_id).await?;
    
    match task {
        Some(task) => Ok(Json(TaskResponse { task })),
        None => Err(ApiError::NotFound("Task not found".to_string())),
    }
}

// ==================== Agents ====================

/// List all agents
async fn list_agents(
    state: State<Arc<AppState>>,
) -> Result<Json<AgentListResponse>> {
    let agents = state.agent_store.list_agents().await?;
    let total = agents.len();
    
    Ok(Json(AgentListResponse {
        agents,
        total,
    }))
}

/// Get agent details
async fn get_agent(
    state: State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentResponse>> {
    let agent = state.agent_store.get_agent(&agent_id).await?;
    
    match agent {
        Some(agent) => Ok(Json(AgentResponse { agent })),
        None => Err(ApiError::NotFound("Agent not found".to_string())),
    }
}

// ==================== Flakes ====================

/// Analyze a flake
async fn analyze_flake(
    state: State<Arc<AppState>>,
    Json(request): Json<AnalyzeFlakeRequest>,
) -> Result<Json<FlakeAnalysisResponse>> {
    let task_id = Uuid::new_v4().to_string();
    
    // Send analysis request
    state.sender.send(AgentMessage::AnalyzeFlake {
        flake_url: request.flake_url.clone(),
        flake_ref: request.flake_ref.clone(),
        task_id: task_id.clone(),
    }).await
        .map_err(|e| ApiError::InternalServerError(e.to_string()))?;
    
    Ok(Json(FlakeAnalysisResponse {
        task_id,
        flake_url: request.flake_url,
        status: "queued".to_string(),
        outputs: vec![],
    }))
}

/// Get flake outputs
async fn get_flake_outputs() -> Result<Json<NixOutputsResponse>> {
    Ok(Json(NixOutputsResponse {
        outputs: vec![],
    }))
}

// ==================== Webhooks ====================

/// Handle GitHub webhook
async fn handle_github_webhook() -> Result<Json<WebhookResponse>> {
    Ok(Json(WebhookResponse {
        received: true,
        action: "none".to_string(),
    }))
}

/// Handle GitLab webhook
async fn handle_gitlab_webhook() -> Result<Json<WebhookResponse>> {
    Ok(Json(WebhookResponse {
        received: true,
        action: "none".to_string(),
    }))
}

/// Handle Forgejo webhook
async fn handle_forgejo_webhook() -> Result<Json<WebhookResponse>> {
    Ok(Json(WebhookResponse {
        received: true,
        action: "none".to_string(),
    }))
}

// ==================== Docs ====================

/// API documentation (HTML)
async fn api_docs() -> &'static str {
    "<h1>AgentFlow API Documentation</h1><p>See <a href='/api/v1/docs.json'>/api/v1/docs.json</a> for JSON spec.</p>"
}

/// API documentation (JSON)
async fn api_docs_json() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "openapi": "3.0.0",
        "info": {
            "title": "AgentFlow API",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "paths": {},
    }))
}

// ==================== Error Handling ====================

/// 404 handler
async fn handler_404() -> impl IntoResponse {
    Json(crate::error::ErrorResponse {
        error: "Not Found".to_string(),
        message: "The requested resource was not found".to_string(),
        details: None,
        code: None,
    })
}

// ==================== Response Types ====================

#[derive(Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Deserialize)]
pub struct SystemStatusResponse {
    pub tasks_total: usize,
    pub tasks_pending: usize,
    pub tasks_running: usize,
    pub agents_total: usize,
    pub agents_active: usize,
    pub uptime: std::time::Duration,
}

#[derive(Serialize, Deserialize)]
pub struct TaskListResponse {
    pub tasks: Vec<TaskDefinition>,
    pub total: usize,
    pub limit: Option<usize>,
    pub offset: usize,
}

#[derive(Serialize, Deserialize)]
pub struct TaskResponse {
    pub task: TaskDefinition,
}

#[derive(Serialize, Deserialize)]
pub struct AgentListResponse {
    pub agents: Vec<agentflow_core::AgentDefinition>,
    pub total: usize,
}

#[derive(Serialize, Deserialize)]
pub struct AgentResponse {
    pub agent: agentflow_core::AgentDefinition,
}

#[derive(Serialize, Deserialize)]
pub struct FlakeAnalysisResponse {
    pub task_id: String,
    pub flake_url: String,
    pub status: String,
    pub outputs: Vec<NixOutput>,
}

#[derive(Serialize, Deserialize)]
pub struct NixOutputsResponse {
    pub outputs: Vec<NixOutput>,
}

#[derive(Serialize, Deserialize)]
pub struct WebhookResponse {
    pub received: bool,
    pub action: String,
}

#[derive(Serialize, Deserialize)]
pub struct DeleteResponse {
    pub deleted: bool,
    pub id: String,
}

// ==================== Request Types ====================

#[derive(Serialize, Deserialize)]
pub struct ListTasksQuery {
    pub status: Option<Vec<TaskStatus>>,
    pub task_type: Option<Vec<TaskType>>,
    pub priority_min: Option<u8>,
    pub priority_max: Option<u8>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Serialize, Deserialize)]
pub struct CreateTaskRequest {
    pub task_type: TaskType,
    pub flake_url: Option<String>,
    pub flake_ref: Option<String>,
    pub system: Option<String>,
    pub targets: Option<Vec<String>>,
    pub depends_on: Option<Vec<String>>,
    pub priority: Option<u8>,
}

#[derive(Serialize, Deserialize)]
pub struct UpdateTaskRequest {
    pub status: Option<TaskStatus>,
    pub priority: Option<u8>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize, Deserialize)]
pub struct AnalyzeFlakeRequest {
    pub flake_url: String,
    pub flake_ref: Option<String>,
}
