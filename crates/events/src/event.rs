//! Event types and structures.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Who produced this event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Human,
    Orchestrator,
    Scheduler,
    Agent,
    System,
}

/// Event type following colon-delimited convention.
///
/// Supports patterns like `task:state:running`, `agent:message`, `merge:completed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum EventType {
    // Task events
    TaskCreated,
    TaskStateRunning,
    TaskStateQuestion,
    TaskStateWaiting,
    TaskStateBlocked,
    TaskStateTesting,
    TaskStateAwaitingMerge,
    TaskStateConflict,
    TaskStateCompleted,
    TaskStateFailed,
    TaskStateCancelled,

    // Agent events
    AgentMessage,
    AgentQuestion,
    AgentError,

    // Merge events
    MergeQueued,
    MergeApproved,
    MergeRejected,
    MergeCompleted,
    MergeConflict,

    // Orchestrator events
    OrchestratorFeedback,
    OrchestratorEscalation,
    OrchestratorDecision,

    // System events
    SystemStarted,
    SystemModePlay,
    SystemModePause,
    SystemModeStop,
    SystemFlush,
    SystemConfigReloaded,
    SystemSchedulerTick,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TaskCreated => "task:created",
            Self::TaskStateRunning => "task:state:running",
            Self::TaskStateQuestion => "task:state:question",
            Self::TaskStateWaiting => "task:state:waiting",
            Self::TaskStateBlocked => "task:state:blocked",
            Self::TaskStateTesting => "task:state:testing",
            Self::TaskStateAwaitingMerge => "task:state:awaiting_merge",
            Self::TaskStateConflict => "task:state:conflict",
            Self::TaskStateCompleted => "task:state:completed",
            Self::TaskStateFailed => "task:state:failed",
            Self::TaskStateCancelled => "task:state:cancelled",
            Self::AgentMessage => "agent:message",
            Self::AgentQuestion => "agent:question",
            Self::AgentError => "agent:error",
            Self::MergeQueued => "merge:queued",
            Self::MergeApproved => "merge:approved",
            Self::MergeRejected => "merge:rejected",
            Self::MergeCompleted => "merge:completed",
            Self::MergeConflict => "merge:conflict",
            Self::OrchestratorFeedback => "orchestrator:feedback",
            Self::OrchestratorEscalation => "orchestrator:escalation",
            Self::OrchestratorDecision => "orchestrator:decision",
            Self::SystemStarted => "system:started",
            Self::SystemModePlay => "system:mode:play",
            Self::SystemModePause => "system:mode:pause",
            Self::SystemModeStop => "system:mode:stop",
            Self::SystemFlush => "system:flush",
            Self::SystemConfigReloaded => "system:config:reloaded",
            Self::SystemSchedulerTick => "system:scheduler:tick",
        }
    }

    /// Check if this event type matches a pattern.
    ///
    /// Patterns support wildcards: `task:*` matches all task events,
    /// `task:state:*` matches all state changes.
    pub fn matches(&self, pattern: &str) -> bool {
        let type_str = self.as_str();

        if pattern == "*" {
            return true;
        }

        if pattern.ends_with(":*") {
            let prefix = &pattern[..pattern.len() - 1]; // Keep the trailing colon
            type_str.starts_with(prefix)
        } else {
            type_str == pattern
        }
    }
}

impl From<EventType> for String {
    fn from(t: EventType) -> Self {
        t.as_str().to_string()
    }
}

impl TryFrom<String> for EventType {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_str() {
            "task:created" => Ok(Self::TaskCreated),
            "task:state:running" => Ok(Self::TaskStateRunning),
            "task:state:question" => Ok(Self::TaskStateQuestion),
            "task:state:waiting" => Ok(Self::TaskStateWaiting),
            "task:state:blocked" => Ok(Self::TaskStateBlocked),
            "task:state:testing" => Ok(Self::TaskStateTesting),
            "task:state:awaiting_merge" => Ok(Self::TaskStateAwaitingMerge),
            "task:state:conflict" => Ok(Self::TaskStateConflict),
            "task:state:completed" => Ok(Self::TaskStateCompleted),
            "task:state:failed" => Ok(Self::TaskStateFailed),
            "task:state:cancelled" => Ok(Self::TaskStateCancelled),
            "agent:message" => Ok(Self::AgentMessage),
            "agent:question" => Ok(Self::AgentQuestion),
            "agent:error" => Ok(Self::AgentError),
            "merge:queued" => Ok(Self::MergeQueued),
            "merge:approved" => Ok(Self::MergeApproved),
            "merge:rejected" => Ok(Self::MergeRejected),
            "merge:completed" => Ok(Self::MergeCompleted),
            "merge:conflict" => Ok(Self::MergeConflict),
            "orchestrator:feedback" => Ok(Self::OrchestratorFeedback),
            "orchestrator:escalation" => Ok(Self::OrchestratorEscalation),
            "orchestrator:decision" => Ok(Self::OrchestratorDecision),
            "system:started" => Ok(Self::SystemStarted),
            "system:mode:play" => Ok(Self::SystemModePlay),
            "system:mode:pause" => Ok(Self::SystemModePause),
            "system:mode:stop" => Ok(Self::SystemModeStop),
            "system:flush" => Ok(Self::SystemFlush),
            "system:config:reloaded" => Ok(Self::SystemConfigReloaded),
            "system:scheduler:tick" => Ok(Self::SystemSchedulerTick),
            _ => Err(format!("unknown event type: {}", s)),
        }
    }
}

/// An event in the system.
///
/// Events are immutable once created. They form an append-only log
/// that provides a complete audit trail of all activity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Unique event ID.
    pub id: Uuid,
    /// Event type (colon-delimited).
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// Task ID this event belongs to.
    pub task: String,
    /// Who produced this event.
    pub actor: Actor,
    /// When the event occurred.
    pub ts: DateTime<Utc>,
    /// Event-type-specific payload.
    pub data: serde_json::Value,
}

impl Event {
    /// Create a new event with the current timestamp.
    pub fn new(
        event_type: EventType,
        task: impl Into<String>,
        actor: Actor,
        data: serde_json::Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            event_type,
            task: task.into(),
            actor,
            ts: Utc::now(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_matches_exact() {
        let t = EventType::TaskCreated;
        assert!(t.matches("task:created"));
        assert!(!t.matches("task:state:running"));
    }

    #[test]
    fn event_type_matches_wildcard() {
        let t = EventType::TaskStateRunning;
        assert!(t.matches("task:*"));
        assert!(t.matches("task:state:*"));
        assert!(!t.matches("agent:*"));
    }

    #[test]
    fn event_type_matches_star() {
        let t = EventType::AgentMessage;
        assert!(t.matches("*"));
    }

    #[test]
    fn event_serializes_type_as_string() {
        let e = Event::new(
            EventType::TaskCreated,
            "task-123",
            Actor::System,
            serde_json::json!({}),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"task:created\""));
    }
}
