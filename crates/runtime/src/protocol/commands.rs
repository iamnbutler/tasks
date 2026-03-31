//! Commands sent from host to container supervisor.

use serde::{Deserialize, Serialize};

/// Agent configuration passed to the container supervisor.
///
/// Controls which tools the agent can use, which model it runs on,
/// and how many conversation turns it gets. Extracted from an
/// `AgentDefinition` by the orchestrator before dispatch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStartConfig {
    /// Agent type identifier (e.g., "implementer", "reviewer").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,

    /// Tool allowlist — only these tools are available to the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,

    /// Tool denylist — these tools are blocked for the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,

    /// Model override for the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Maximum conversation turns before the agent is stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

/// Start the agent with repo setup and initial prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartCommand {
    pub repo: String,
    pub branch: String,
    pub prompt: String,
    /// Optional agent configuration for tool restrictions, model, and limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<AgentStartConfig>,
}

/// Send a chat message to the running agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCommand {
    pub text: String,
}

/// Execute an arbitrary command in the container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecCommand {
    pub id: String,
    pub argv: Vec<String>,
}

/// All commands that can be sent to the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Start(StartCommand),
    Chat(ChatCommand),
    Stop,
    Exec(ExecCommand),
}

impl Command {
    pub fn start(repo: impl Into<String>, branch: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self::Start(StartCommand {
            repo: repo.into(),
            branch: branch.into(),
            prompt: prompt.into(),
            agent_config: None,
        })
    }

    pub fn start_with_config(
        repo: impl Into<String>,
        branch: impl Into<String>,
        prompt: impl Into<String>,
        agent_config: AgentStartConfig,
    ) -> Self {
        Self::Start(StartCommand {
            repo: repo.into(),
            branch: branch.into(),
            prompt: prompt.into(),
            agent_config: Some(agent_config),
        })
    }

    pub fn chat(text: impl Into<String>) -> Self {
        Self::Chat(ChatCommand { text: text.into() })
    }

    pub fn stop() -> Self {
        Self::Stop
    }

    pub fn exec(id: impl Into<String>, argv: Vec<String>) -> Self {
        Self::Exec(ExecCommand {
            id: id.into(),
            argv,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_start() {
        let cmd = Command::start("owner/repo", "main", "do the thing");
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"cmd\":\"start\""));
        assert!(json.contains("\"repo\":\"owner/repo\""));
    }

    #[test]
    fn serialize_stop() {
        let cmd = Command::stop();
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"cmd":"stop"}"#);
    }

    #[test]
    fn roundtrip() {
        let cmd = Command::chat("hello");
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: Command = serde_json::from_str(&json).unwrap();
        match parsed {
            Command::Chat(c) => assert_eq!(c.text, "hello"),
            _ => panic!("wrong variant"),
        }
    }
}
