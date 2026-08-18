use thiserror::Error;

/// Top-level error type for AgentFlow
#[derive(Debug, Error)]
pub enum AgentFlowError {
    /// Channel send error
    #[error("Channel send error: {0}")]
    ChannelSend(#[from] tokio::sync::mpsc::error::SendError<crate::message::AgentMessage>),
    /// Generic error with message
    #[error("{0}")]
    Generic(String),
    
    /// Timeout error
    #[error("Operation timed out")]
    Timeout,
    
    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    /// Serde serialization error
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    
    /// Task-specific error
    #[error("Task error: {0}")]
    Task(String),
    
    /// Agent error
    #[error("Agent error: {0}")]
    Agent(String),
    
    /// Storage error
    #[error("Storage error: {0}")]
    Storage(String),
    
    /// Network error
    #[error("Network error: {0}")]
    Network(String),
    
    /// Authentication/authorization error
    #[error("Auth error: {0}")]
    Auth(String),
    
    /// Not found
    #[error("{0} not found")]
    NotFound(String),
    
    /// Already exists
    #[error("{0} already exists")]
    AlreadyExists(String),
}

pub type Result<T, E = AgentFlowError> = std::result::Result<T, E>;
