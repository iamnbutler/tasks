//! Commands sent from host to container supervisor.

use serde::{Deserialize, Serialize};

/// Start the agent with repo setup and initial prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartCommand {
    pub repo: String,
    pub branch: String,
    pub prompt: String,
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
