//! Server configuration

use serde::Deserialize;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::error::Result;
use crate::ApiError;

/// Server configuration loaded from environment variables
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Bind address (e.g., 0.0.0.0:8080)
    pub bind_address: SocketAddr,
    
    /// NATS server URL (optional, for distributed mode)
    pub nats_url: Option<String>,
    
    /// NATS authentication token (optional)
    pub nats_token: Option<String>,
    
    /// Debug mode
    pub debug: bool,
    
    /// Log level (trace, debug, info, warn, error)
    pub log_level: String,
    
    /// Database URL (optional, for persistent storage)
    pub database_url: Option<String>,
    
    /// S3 endpoint for artifact storage (optional)
    pub s3_endpoint: Option<String>,
    
    /// S3 access key (optional)
    pub s3_access_key: Option<String>,
    
    /// S3 secret key (optional)
    pub s3_secret_key: Option<String>,
    
    /// S3 bucket name (optional)
    pub s3_bucket: Option<String>,
    
    /// Matrix homeserver URL (optional)
    pub matrix_homeserver: Option<String>,
    
    /// Matrix username (optional)
    pub matrix_user: Option<String>,
    
    /// Matrix password (optional)
    pub matrix_password: Option<String>,
    
    /// GitHub webhook secret (optional)
    pub github_webhook_secret: Option<String>,
    
    /// GitLab webhook secret (optional)
    pub gitlab_webhook_secret: Option<String>,
    
    /// Forgejo webhook secret (optional)
    pub forgejo_webhook_secret: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0:8080".parse().expect("Invalid default bind address"),
            nats_url: None,
            nats_token: None,
            debug: false,
            log_level: "info".to_string(),
            database_url: None,
            s3_endpoint: None,
            s3_access_key: None,
            s3_secret_key: None,
            s3_bucket: None,
            matrix_homeserver: None,
            matrix_user: None,
            matrix_password: None,
            github_webhook_secret: None,
            gitlab_webhook_secret: None,
            forgejo_webhook_secret: None,
        }
    }
}

impl ServerConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        let mut config = Self::default();
        
        // Load from environment with overrides
        if let Ok(addr) = env::var("AGENTFLOW_BIND_ADDRESS") {
            config.bind_address = addr.parse()
                .map_err(|_| ApiError::Configuration("Invalid bind address".to_string()))?;
        }
        
        if let Ok(nats_url) = env::var("AGENTFLOW_NATS_URL") {
            config.nats_url = Some(nats_url);
        }
        
        if let Ok(nats_token) = env::var("AGENTFLOW_NATS_TOKEN") {
            config.nats_token = Some(nats_token);
        }
        
        if let Ok(debug) = env::var("AGENTFLOW_DEBUG") {
            config.debug = debug.parse().unwrap_or(false);
        }
        
        if let Ok(log_level) = env::var("AGENTFLOW_LOG_LEVEL") {
            config.log_level = log_level;
        }
        
        if let Ok(db_url) = env::var("AGENTFLOW_DATABASE_URL") {
            config.database_url = Some(db_url);
        }
        
        if let Ok(s3_endpoint) = env::var("AGENTFLOW_S3_ENDPOINT") {
            config.s3_endpoint = Some(s3_endpoint);
        }
        
        if let Ok(s3_access_key) = env::var("AGENTFLOW_S3_ACCESS_KEY") {
            config.s3_access_key = Some(s3_access_key);
        }
        
        if let Ok(s3_secret_key) = env::var("AGENTFLOW_S3_SECRET_KEY") {
            config.s3_secret_key = Some(s3_secret_key);
        }
        
        if let Ok(s3_bucket) = env::var("AGENTFLOW_S3_BUCKET") {
            config.s3_bucket = Some(s3_bucket);
        }
        
        if let Ok(matrix_homeserver) = env::var("AGENTFLOW_MATRIX_HOMESERVER") {
            config.matrix_homeserver = Some(matrix_homeserver);
        }
        
        if let Ok(matrix_user) = env::var("AGENTFLOW_MATRIX_USER") {
            config.matrix_user = Some(matrix_user);
        }
        
        if let Ok(matrix_password) = env::var("AGENTFLOW_MATRIX_PASSWORD") {
            config.matrix_password = Some(matrix_password);
        }
        
        if let Ok(secret) = env::var("AGENTFLOW_GITHUB_WEBHOOK_SECRET") {
            config.github_webhook_secret = Some(secret);
        }
        
        if let Ok(secret) = env::var("AGENTFLOW_GITLAB_WEBHOOK_SECRET") {
            config.gitlab_webhook_secret = Some(secret);
        }
        
        if let Ok(secret) = env::var("AGENTFLOW_FORGEJO_WEBHOOK_SECRET") {
            config.forgejo_webhook_secret = Some(secret);
        }
        
        Ok(config)
    }
    
    /// Load configuration from a file (optional)
    #[allow(dead_code)]
    pub fn from_file(path: &PathBuf) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ApiError::Configuration(format!("Failed to read config file: {}", e)))?;
        
        let mut config: Self = serde_yaml::from_str(&content)
            .map_err(|e| ApiError::Configuration(format!("Failed to parse config file: {}", e)))?;
        
        // Apply environment variable overrides
        if let Ok(addr) = env::var("AGENTFLOW_BIND_ADDRESS") {
            config.bind_address = addr.parse()
                .map_err(|_| ApiError::Configuration("Invalid bind address".to_string()))?;
        }
        
        // Apply other overrides as needed
        
        Ok(config)
    }
    
    /// Get NATS URL or return error if not configured in distributed mode
    #[allow(dead_code)]
    pub fn nats_url(&self) -> Result<String> {
        self.nats_url.clone()
            .ok_or_else(|| ApiError::Configuration("NATS URL not configured".to_string()))
    }
    
    /// Check if distributed mode is enabled
    #[allow(dead_code)]
    pub fn is_distributed(&self) -> bool {
        self.nats_url.is_some()
    }
    
    /// Get S3 configuration if enabled
    #[allow(dead_code)]
    pub fn s3_config(&self) -> Option<S3Config> {
        if let (Some(endpoint), Some(access_key), Some(secret_key), Some(bucket)) = (
            &self.s3_endpoint,
            &self.s3_access_key,
            &self.s3_secret_key,
            &self.s3_bucket,
        ) {
            Some(S3Config {
                endpoint: endpoint.clone(),
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                bucket: bucket.clone(),
            })
        } else {
            None
        }
    }
}

/// S3 configuration
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct S3Config {
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
    pub bucket: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_default_config() {
        let config = ServerConfig::default();
        assert_eq!(config.bind_address, "0.0.0.0:8080".parse().unwrap());
        assert!(!config.debug);
        assert_eq!(config.log_level, "info");
    }
    
    #[test]
    fn test_from_env() {
        env::set_var("AGENTFLOW_BIND_ADDRESS", "127.0.0.1:3000");
        env::set_var("AGENTFLOW_DEBUG", "true");
        env::set_var("AGENTFLOW_LOG_LEVEL", "debug");
        
        let config = ServerConfig::from_env().unwrap();
        assert_eq!(config.bind_address, "127.0.0.1:3000".parse().unwrap());
        assert!(config.debug);
        assert_eq!(config.log_level, "debug");
        
        env::remove_var("AGENTFLOW_BIND_ADDRESS");
        env::remove_var("AGENTFLOW_DEBUG");
        env::remove_var("AGENTFLOW_LOG_LEVEL");
    }
}
