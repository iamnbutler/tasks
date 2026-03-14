//! Container runtime abstraction.

use std::collections::HashMap;
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;

use crate::transport::StdioTransport;

#[derive(Debug, Error)]
pub enum ContainerError {
    #[error("container command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for creating a container.
#[derive(Debug, Clone)]
pub struct ContainerConfig {
    pub image: String,
    pub env: HashMap<String, String>,
    pub cpus: Option<f32>,
    pub memory: Option<String>,
}

impl ContainerConfig {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            env: HashMap::new(),
            cpus: None,
            memory: None,
        }
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn cpus(mut self, cpus: f32) -> Self {
        self.cpus = Some(cpus);
        self
    }

    pub fn memory(mut self, memory: impl Into<String>) -> Self {
        self.memory = Some(memory.into());
        self
    }
}

/// Trait for container lifecycle operations.
///
/// Implementations can use different container runtimes (apple/container, Docker, etc.)
#[trait_variant::make(Send)]
pub trait ContainerRuntime {
    /// Create a container from the given config. Returns the container ID.
    async fn create(&self, config: &ContainerConfig) -> Result<String, ContainerError>;
    /// Start a container and attach to its stdio. Returns the transport.
    async fn start(&self, container_id: &str) -> Result<StdioTransport, ContainerError>;
    /// Stop a running container.
    async fn stop(&self, container_id: &str) -> Result<(), ContainerError>;
    /// Destroy a container and clean up.
    async fn destroy(&self, container_id: &str) -> Result<(), ContainerError>;
}

/// Container runtime using the apple/container CLI.
///
/// See https://github.com/apple/container
#[derive(Clone)]
pub struct AppleContainerRuntime;

impl AppleContainerRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AppleContainerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ContainerRuntime for AppleContainerRuntime {
    async fn create(&self, config: &ContainerConfig) -> Result<String, ContainerError> {
        let mut cmd = Command::new("container");
        cmd.arg("create");

        for (key, value) in &config.env {
            cmd.arg("-e").arg(format!("{}={}", key, value));
        }

        if let Some(cpus) = config.cpus {
            cmd.arg("-c").arg(cpus.to_string());
        }

        if let Some(ref memory) = config.memory {
            cmd.arg("-m").arg(memory);
        }

        // Image is a positional argument (must come last, before any container arguments).
        cmd.arg(&config.image);

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ContainerError::CommandFailed(stderr.to_string()));
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(container_id)
    }

    /// Start the container with attached stdio.
    ///
    /// Uses `container start --attach --interactive <id>` which starts the
    /// container and connects stdin/stdout to the supervisor process.
    async fn start(&self, container_id: &str) -> Result<StdioTransport, ContainerError> {
        let child = std::process::Command::new("container")
            .arg("start")
            .arg("--attach")
            .arg("--interactive")
            .arg(container_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Ok(StdioTransport::new(child))
    }

    async fn stop(&self, container_id: &str) -> Result<(), ContainerError> {
        let output = Command::new("container")
            .arg("stop")
            .arg(container_id)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ContainerError::CommandFailed(stderr.to_string()));
        }

        Ok(())
    }

    async fn destroy(&self, container_id: &str) -> Result<(), ContainerError> {
        // Stop first (ignore errors — may already be stopped)
        let _ = self.stop(container_id).await;

        let output = Command::new("container")
            .arg("rm")
            .arg(container_id)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ContainerError::CommandFailed(stderr.to_string()));
        }

        Ok(())
    }
}
