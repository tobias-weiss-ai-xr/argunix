//! Error types for the AgentFlow server

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Serialize;
use std::fmt;

/// Result type alias for AgentFlow server
pub type Result<T, E = ApiError> = std::result::Result<T, E>;

/// API Error type
#[derive(Debug)]
pub enum ApiError {
    /// Not found
    NotFound(String),
    
    /// Bad request
    BadRequest(String),
    
    /// Unauthorized
    #[allow(dead_code)]
    Unauthorized(String),
    
    /// Forbidden
    #[allow(dead_code)]
    Forbidden(String),
    
    /// Conflict
    Conflict(String),
    
    /// Internal server error
    InternalServerError(String),
    
    /// Configuration error
    Configuration(String),
    
    /// Database error
    #[allow(dead_code)]
    Database(String),
    
    /// Storage error
    Storage(String),
    
    /// Authentication error
    Authentication(String),
    
    /// Rate limit exceeded
    #[allow(dead_code)]
    RateLimitExceeded,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::NotFound(msg) => write!(f, "Not found: {}", msg),
            ApiError::BadRequest(msg) => write!(f, "Bad request: {}", msg),
            ApiError::Unauthorized(msg) => write!(f, "Unauthorized: {}", msg),
            ApiError::Forbidden(msg) => write!(f, "Forbidden: {}", msg),
            ApiError::Conflict(msg) => write!(f, "Conflict: {}", msg),
            ApiError::InternalServerError(msg) => write!(f, "Internal server error: {}", msg),
            ApiError::Configuration(msg) => write!(f, "Configuration error: {}", msg),
            ApiError::Database(msg) => write!(f, "Database error: {}", msg),
            ApiError::Storage(msg) => write!(f, "Storage error: {}", msg),
            ApiError::Authentication(msg) => write!(f, "Authentication error: {}", msg),
            ApiError::RateLimitExceeded => write!(f, "Rate limit exceeded"),
        }
    }
}

impl std::error::Error for ApiError {}

// Implement From for std::io::Error
impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        ApiError::InternalServerError(error.to_string())
    }
}

/// Error response structure
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
    pub details: Option<serde_json::Value>,
    pub code: Option<String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error, message) = match self {
            ApiError::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                "not_found",
                msg,
            ),
            ApiError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                "bad_request",
                msg,
            ),
            ApiError::Unauthorized(msg) => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                msg,
            ),
            ApiError::Forbidden(msg) => (
                StatusCode::FORBIDDEN,
                "forbidden",
                msg,
            ),
            ApiError::Conflict(msg) => (
                StatusCode::CONFLICT,
                "conflict",
                msg,
            ),
            ApiError::InternalServerError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_server_error",
                msg,
            ),
            ApiError::Configuration(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "configuration_error",
                msg,
            ),
            ApiError::Database(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "database_error",
                msg,
            ),
            ApiError::Storage(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                msg,
            ),
            ApiError::Authentication(msg) => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                msg,
            ),
            ApiError::RateLimitExceeded => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                "Too many requests".to_string(),
            ),
        };
        
        let body = ErrorResponse {
            error: error.to_string(),
            message,
            details: None,
            code: Some(error.to_string()),
        };
        
        (status, Json(body)).into_response()
    }
}

// Implement From for agentflow_core::AgentFlowError
impl From<agentflow_core::AgentFlowError> for ApiError {
    fn from(error: agentflow_core::AgentFlowError) -> Self {
        match error {
            agentflow_core::AgentFlowError::Generic(msg) => ApiError::InternalServerError(msg),
            agentflow_core::AgentFlowError::Io(_) => ApiError::InternalServerError("IO error".to_string()),
            agentflow_core::AgentFlowError::Serde(_) => ApiError::BadRequest("Serialization error".to_string()),
            agentflow_core::AgentFlowError::Task(msg) => ApiError::InternalServerError(msg),
            agentflow_core::AgentFlowError::Agent(msg) => ApiError::InternalServerError(msg),
            agentflow_core::AgentFlowError::Storage(msg) => ApiError::Storage(msg),
            agentflow_core::AgentFlowError::Network(msg) => ApiError::InternalServerError(msg),
            agentflow_core::AgentFlowError::Auth(msg) => ApiError::Authentication(msg),
            agentflow_core::AgentFlowError::NotFound(resource) => ApiError::NotFound(resource),
            agentflow_core::AgentFlowError::AlreadyExists(resource) => ApiError::Conflict(resource),
            agentflow_core::AgentFlowError::ChannelSend(_) => ApiError::InternalServerError("Message bus error".to_string()),
        }
    }
}

// Implement From for Storage errors
impl From<Box<dyn std::error::Error + Send + Sync>> for ApiError {
    fn from(error: Box<dyn std::error::Error + Send + Sync>) -> Self {
        ApiError::InternalServerError(error.to_string())
    }
}

// Convenience constructor for internal errors
impl ApiError {
    pub fn internal<E: std::fmt::Display>(error: E) -> Self {
        ApiError::InternalServerError(error.to_string())
    }
    
    #[allow(dead_code)]
    pub fn bad_request<E: std::fmt::Display>(error: E) -> Self {
        ApiError::BadRequest(error.to_string())
    }
    
    #[allow(dead_code)]
    pub fn not_found<E: std::fmt::Display>(error: E) -> Self {
        ApiError::NotFound(error.to_string())
    }
}
