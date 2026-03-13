//! Events emitted from container supervisor to host.

use serde::{Deserialize, Serialize};

/// Supervisor is ready to accept commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemReadyEvent {}

/// Agent process has started.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStartedEvent {
    pub pid: u32,
}

/// Agent wrote to stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStdoutEvent {
    pub data: String,
}

/// Agent wrote to stderr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStderrEvent {
    pub data: String,
}

/// Agent process exited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExitEvent {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

/// Result of an exec command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResultEvent {
    pub id: String,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// All events that can be received from the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    #[serde(rename = "system:ready")]
    SystemReady(SystemReadyEvent),
    #[serde(rename = "agent:started")]
    AgentStarted(AgentStartedEvent),
    #[serde(rename = "agent:stdout")]
    AgentStdout(AgentStdoutEvent),
    #[serde(rename = "agent:stderr")]
    AgentStderr(AgentStderrEvent),
    #[serde(rename = "agent:exit")]
    AgentExit(AgentExitEvent),
    #[serde(rename = "exec:result")]
    ExecResult(ExecResultEvent),
}

impl Event {
    /// Check if this is a system:ready event.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::SystemReady(_))
    }

    /// Check if this is an agent:exit event.
    pub fn is_exit(&self) -> bool {
        matches!(self, Self::AgentExit(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_ready() {
        let json = r#"{"ev":"system:ready"}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        assert!(event.is_ready());
    }

    #[test]
    fn deserialize_stdout() {
        let json = r#"{"ev":"agent:stdout","data":"hello world"}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::AgentStdout(e) => assert_eq!(e.data, "hello world"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn deserialize_exit() {
        let json = r#"{"ev":"agent:exit","code":0,"signal":null}"#;
        let event: Event = serde_json::from_str(json).unwrap();
        match event {
            Event::AgentExit(e) => {
                assert_eq!(e.code, Some(0));
                assert_eq!(e.signal, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn serialize_roundtrip() {
        let event = Event::AgentStarted(AgentStartedEvent { pid: 1234 });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"ev\":\"agent:started\""));
        let parsed: Event = serde_json::from_str(&json).unwrap();
        match parsed {
            Event::AgentStarted(e) => assert_eq!(e.pid, 1234),
            _ => panic!("wrong variant"),
        }
    }
}
