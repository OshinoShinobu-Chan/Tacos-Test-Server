//! # Config
//!
//! This module contains configuration structures and functions for the Tacos Test Server.
use std::collections::HashMap;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ServerConfig {
    /// The address to bind the server to
    /// Default: `[0, 0, 0, 0, 0, 0, 0, 0]`
    pub bind_address: String,
    /// The port to bind the server to
    /// Default: `3000`
    pub bind_port: u16,
    /// The maximum number of concurrent requests the server can handle
    /// Default: `16`
    pub concurrency: usize,
    /// Maximum size of the channel between threads
    /// Default: `1024`
    pub channel_size: usize,
    /// Temp file dir
    /// Default: `/tmp`
    pub tmp_file_dir: String,
    /// Upload file size limit in bytes
    /// Default: `5242880` (5 MB)
    pub upload_file_size_limit_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind_address: "[::]".to_string(),
            bind_port: 3000,
            concurrency: 16,
            channel_size: 1024,
            tmp_file_dir: "/tmp".to_string(),
            upload_file_size_limit_bytes: 5 * 1024 * 1024, // 5 MB
        }
    }
}

impl ServerConfig {
    /// Get the full bind address as a string
    pub fn get_bind_addr(&self) -> String {
        format!("{}:{}", self.bind_address, self.bind_port)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct QueueConfig {
    /// Maximum number of requests in the queue
    /// Default: `100`
    pub max_requests: usize,
    /// Maximum number of results to keep
    /// Default: `1000`
    pub max_results: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            max_requests: 100,
            max_results: 1000,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TestConfig {
    /// Time limit for each test in seconds
    /// Default: `1800` (30 minutes)
    pub time_limit_secs: u64,
    /// Lab command mapping
    /// Key: lab name
    /// Value: command and its arguments
    pub lab_command: HashMap<String, Vec<String>>,
}

impl Default for TestConfig {
    fn default() -> Self {
        TestConfig {
            time_limit_secs: 1800,
            lab_command: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub queue: QueueConfig,
    pub test: TestConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: ServerConfig::default(),
            queue: QueueConfig::default(),
            test: TestConfig::default(),
        }
    }
}

impl Config {
    pub fn from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
