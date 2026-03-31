//! Session wrapper — high-level API for managing a session.

use std::collections::VecDeque;
use std::time::Duration;

use thiserror::Error;

use crate::container::{ContainerConfig, ContainerError, ContainerRuntime};
use crate::protocol::{Command, Event};
use crate::transport::{StdioTransport, TransportError};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("container error: {0}")]
    Container(#[from] ContainerError),
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("session not started")]
    NotStarted,
    #[error("timeout waiting for ready")]
    ReadyTimeout,
}

/// Session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Session created but not started.
    Created,
    /// Container running, waiting for supervisor ready.
    Starting,
    /// Supervisor ready, can accept commands.
    Ready,
    /// Agent is running.
    Running,
    /// Session has ended.
    Ended,
}

/// High-level session wrapper.
///
/// Manages the full lifecycle of a container session: create, start,
/// wait for ready, send commands, receive events, and cleanup.
pub struct Session<R: ContainerRuntime> {
    runtime: R,
    config: ContainerConfig,
    container_id: Option<String>,
    transport: Option<StdioTransport>,
    state: SessionState,
    /// Events buffered during wait_ready() that arrived before SystemReady.
    buffered_events: VecDeque<Event>,
}

impl<R: ContainerRuntime> Session<R> {
    /// Create a new session with the given runtime and config.
    pub fn new(runtime: R, config: ContainerConfig) -> Self {
        Self {
            runtime,
            config,
            container_id: None,
            transport: None,
            state: SessionState::Created,
            buffered_events: VecDeque::new(),
        }
    }

    /// Get the current session state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Get the container ID if created.
    pub fn container_id(&self) -> Option<&str> {
        self.container_id.as_deref()
    }

    /// Create and start the container, wait for supervisor ready.
    pub async fn start(&mut self, ready_timeout: Duration) -> Result<(), SessionError> {
        // Create container
        let container_id = self.runtime.create(&self.config).await?;
        self.container_id = Some(container_id.clone());

        // Start container and attach stdio
        let transport = self.runtime.start(&container_id).await?;
        self.transport = Some(transport);
        self.state = SessionState::Starting;

        // Wait for system:ready
        self.wait_ready(ready_timeout)?;
        self.state = SessionState::Ready;

        Ok(())
    }

    fn wait_ready(&mut self, timeout: Duration) -> Result<(), SessionError> {
        let transport = self.transport.as_ref().ok_or(SessionError::NotStarted)?;

        loop {
            match transport.recv_timeout(timeout) {
                Ok(event) if event.is_ready() => return Ok(()),
                Ok(event) => self.buffered_events.push_back(event),
                Err(_) => return Err(SessionError::ReadyTimeout),
            }
        }
    }

    /// Start the agent with a repo, branch, and prompt.
    pub fn start_agent(
        &mut self,
        repo: impl Into<String>,
        branch: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Result<(), SessionError> {
        let transport = self.transport.as_mut().ok_or(SessionError::NotStarted)?;
        let cmd = Command::start(repo, branch, prompt);
        transport.send(&cmd)?;
        self.state = SessionState::Running;
        Ok(())
    }

    /// Start the agent with a repo, branch, prompt, and agent configuration.
    ///
    /// The `agent_config` controls tool restrictions, model selection, and
    /// turn limits for this agent session.
    pub fn start_agent_with_config(
        &mut self,
        repo: impl Into<String>,
        branch: impl Into<String>,
        prompt: impl Into<String>,
        agent_config: crate::protocol::AgentStartConfig,
    ) -> Result<(), SessionError> {
        let transport = self.transport.as_mut().ok_or(SessionError::NotStarted)?;
        let cmd = Command::start_with_config(repo, branch, prompt, agent_config);
        transport.send(&cmd)?;
        self.state = SessionState::Running;
        Ok(())
    }

    /// Send a chat message to the agent.
    pub fn send_chat(&mut self, text: impl Into<String>) -> Result<(), SessionError> {
        let transport = self.transport.as_mut().ok_or(SessionError::NotStarted)?;
        transport.send(&Command::chat(text))?;
        Ok(())
    }

    /// Send a stop command to the agent.
    pub fn stop_agent(&mut self) -> Result<(), SessionError> {
        let transport = self.transport.as_mut().ok_or(SessionError::NotStarted)?;
        transport.send(&Command::stop())?;
        Ok(())
    }

    /// Execute a command in the container.
    pub fn exec(&mut self, id: impl Into<String>, argv: Vec<String>) -> Result<(), SessionError> {
        let transport = self.transport.as_mut().ok_or(SessionError::NotStarted)?;
        transport.send(&Command::exec(id, argv))?;
        Ok(())
    }

    /// Try to receive an event without blocking.
    pub fn try_recv(&self) -> Option<Event> {
        self.transport.as_ref()?.try_recv()
    }

    /// Receive an event, blocking until available.
    pub fn recv(&mut self) -> Result<Event, SessionError> {
        if let Some(event) = self.buffered_events.pop_front() {
            return Ok(event);
        }
        let transport = self.transport.as_ref().ok_or(SessionError::NotStarted)?;
        Ok(transport.recv()?)
    }

    /// Receive an event with timeout.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Event, SessionError> {
        if let Some(event) = self.buffered_events.pop_front() {
            return Ok(event);
        }
        let transport = self.transport.as_ref().ok_or(SessionError::NotStarted)?;
        Ok(transport.recv_timeout(timeout)?)
    }

    /// Destroy the session and cleanup.
    pub async fn destroy(mut self) -> Result<(), SessionError> {
        self.state = SessionState::Ended;

        if let Some(transport) = self.transport.take() {
            if let Err(e) = transport.close() {
                tracing::warn!(container_id = ?self.container_id, error = %e, "failed to close transport during session destroy");
            }
        }

        if let Some(ref container_id) = self.container_id {
            self.runtime.destroy(container_id).await?;
        }

        Ok(())
    }
}
