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
    TaskUpdated,
    TaskReordered,
    TaskStateRunning,
    TaskStateQuestion,
    TaskStateWaiting,
    TaskStateBlocked,
    TaskStateTesting,
    TaskStateAwaitingMerge,
    TaskStateConflict,
    TaskStateChangesRequested,
    TaskStateCompleted,
    TaskStateFailed,
    TaskStateCancelled,

    // Agent events
    AgentMessage,
    AgentQuestion,
    AgentError,

    // Human events
    HumanMessage,

    // Merge events
    MergeQueued,
    MergeApproved,
    /// Merge in progress — GitHub API call executing.
    MergeMerging,
    MergeRejected,
    MergeChangesRequested,
    MergeCompleted,
    MergeConflict,

    // Automation events (spec §5.7)
    AutomationCreated,
    AutomationUpdated,
    AutomationDeleted,
    AutomationRunStarted,
    /// Streaming output chunk from an automation run.
    AutomationRunOutput,
    AutomationRunCompleted,
    AutomationRunFailed,
    AutomationRunCancelled,

    // Workspace events
    /// Workspace cleaned up (spec §10.3).
    WorkspaceCleaned,

    // Orchestrator events
    OrchestratorFeedback,
    OrchestratorEscalation,
    OrchestratorDecision,
    OrchestratorMessage,
    /// Orchestrator chat response (LLM-generated reply to human message).
    OrchestratorResponse,
    /// Orchestrator stream-of-consciousness narration of system events.
    OrchestratorThought,

    // System events
    SystemStarted,
    SystemModePlay,
    SystemModePause,
    SystemModeStop,
    SystemFlush,
    SystemConfigReloaded,
    SystemSchedulerTick,
    SystemMemoryWarning,
    SystemMemoryPressure,
    SystemMemoryEmergency,
    /// Session soft time limit reached (spec §17.4).
    SystemTimeLimitSoft,
    /// Session hard time limit reached (spec §17.4).
    SystemTimeLimitHard,
    /// State rebuild from GitHub (issue #256).
    SystemRebuild,

    // Accounting events (spec §16.4)
    /// Token usage accounting (input/output tokens per request).
    SystemAccountingTokens,
    /// External API call tracking.
    SystemAccountingApiCall,
    /// Session duration and total token accounting.
    SystemAccountingSession,

    // Reflection events (spec §8.6)
    /// New reflection issue detected.
    ReflectionCreated,
    /// Comment added to a reflection.
    ReflectionComment,
    /// Reflection resolved/closed.
    ReflectionClosed,

    // Self-update events (issue #305)
    /// New update is available from upstream.
    SystemUpdateAvailable,
    /// Update is being applied (graceful shutdown in progress).
    SystemUpdateApplying,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TaskCreated => "task:created",
            Self::TaskUpdated => "task:updated",
            Self::TaskReordered => "task:reordered",
            Self::TaskStateRunning => "task:state:running",
            Self::TaskStateQuestion => "task:state:question",
            Self::TaskStateWaiting => "task:state:waiting",
            Self::TaskStateBlocked => "task:state:blocked",
            Self::TaskStateTesting => "task:state:testing",
            Self::TaskStateAwaitingMerge => "task:state:awaiting_merge",
            Self::TaskStateConflict => "task:state:conflict",
            Self::TaskStateChangesRequested => "task:state:changes_requested",
            Self::TaskStateCompleted => "task:state:completed",
            Self::TaskStateFailed => "task:state:failed",
            Self::TaskStateCancelled => "task:state:cancelled",
            Self::AgentMessage => "agent:message",
            Self::AgentQuestion => "agent:question",
            Self::AgentError => "agent:error",
            Self::HumanMessage => "human:message",
            Self::MergeQueued => "merge:queued",
            Self::MergeApproved => "merge:approved",
            Self::MergeMerging => "merge:merging",
            Self::MergeRejected => "merge:rejected",
            Self::MergeChangesRequested => "merge:changes_requested",
            Self::MergeCompleted => "merge:completed",
            Self::MergeConflict => "merge:conflict",
            Self::AutomationCreated => "automation:created",
            Self::AutomationUpdated => "automation:updated",
            Self::AutomationDeleted => "automation:deleted",
            Self::AutomationRunStarted => "automation:run:started",
            Self::AutomationRunOutput => "automation:run:output",
            Self::AutomationRunCompleted => "automation:run:completed",
            Self::AutomationRunFailed => "automation:run:failed",
            Self::AutomationRunCancelled => "automation:run:cancelled",
            Self::WorkspaceCleaned => "workspace:cleaned",
            Self::OrchestratorFeedback => "orchestrator:feedback",
            Self::OrchestratorEscalation => "orchestrator:escalation",
            Self::OrchestratorDecision => "orchestrator:decision",
            Self::OrchestratorMessage => "orchestrator:message",
            Self::OrchestratorResponse => "orchestrator:response",
            Self::OrchestratorThought => "orchestrator:thought",
            Self::SystemStarted => "system:started",
            Self::SystemModePlay => "system:mode:play",
            Self::SystemModePause => "system:mode:pause",
            Self::SystemModeStop => "system:mode:stop",
            Self::SystemFlush => "system:flush",
            Self::SystemConfigReloaded => "system:config:reloaded",
            Self::SystemSchedulerTick => "system:scheduler:tick",
            Self::SystemMemoryWarning => "system:memory:warning",
            Self::SystemMemoryPressure => "system:memory:pressure",
            Self::SystemMemoryEmergency => "system:memory:emergency",
            Self::SystemTimeLimitSoft => "system:time_limit:soft",
            Self::SystemTimeLimitHard => "system:time_limit:hard",
            Self::SystemRebuild => "system:rebuild",
            Self::SystemAccountingTokens => "system:accounting:tokens",
            Self::SystemAccountingApiCall => "system:accounting:api_call",
            Self::SystemAccountingSession => "system:accounting:session",
            Self::ReflectionCreated => "reflection:created",
            Self::ReflectionComment => "reflection:comment",
            Self::ReflectionClosed => "reflection:closed",
            Self::SystemUpdateAvailable => "system:update:available",
            Self::SystemUpdateApplying => "system:update:applying",
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
            "task:updated" => Ok(Self::TaskUpdated),
            "task:reordered" => Ok(Self::TaskReordered),
            "task:state:running" => Ok(Self::TaskStateRunning),
            "task:state:question" => Ok(Self::TaskStateQuestion),
            "task:state:waiting" => Ok(Self::TaskStateWaiting),
            "task:state:blocked" => Ok(Self::TaskStateBlocked),
            "task:state:testing" => Ok(Self::TaskStateTesting),
            "task:state:awaiting_merge" => Ok(Self::TaskStateAwaitingMerge),
            "task:state:conflict" => Ok(Self::TaskStateConflict),
            "task:state:changes_requested" => Ok(Self::TaskStateChangesRequested),
            "task:state:completed" => Ok(Self::TaskStateCompleted),
            "task:state:failed" => Ok(Self::TaskStateFailed),
            "task:state:cancelled" => Ok(Self::TaskStateCancelled),
            "agent:message" => Ok(Self::AgentMessage),
            "agent:question" => Ok(Self::AgentQuestion),
            "agent:error" => Ok(Self::AgentError),
            "human:message" => Ok(Self::HumanMessage),
            "merge:queued" => Ok(Self::MergeQueued),
            "merge:approved" => Ok(Self::MergeApproved),
            "merge:merging" => Ok(Self::MergeMerging),
            "merge:rejected" => Ok(Self::MergeRejected),
            "merge:changes_requested" => Ok(Self::MergeChangesRequested),
            "merge:completed" => Ok(Self::MergeCompleted),
            "merge:conflict" => Ok(Self::MergeConflict),
            "automation:created" => Ok(Self::AutomationCreated),
            "automation:updated" => Ok(Self::AutomationUpdated),
            "automation:deleted" => Ok(Self::AutomationDeleted),
            "automation:run:started" => Ok(Self::AutomationRunStarted),
            "automation:run:output" => Ok(Self::AutomationRunOutput),
            "automation:run:completed" => Ok(Self::AutomationRunCompleted),
            "automation:run:failed" => Ok(Self::AutomationRunFailed),
            "automation:run:cancelled" => Ok(Self::AutomationRunCancelled),
            "workspace:cleaned" => Ok(Self::WorkspaceCleaned),
            "orchestrator:feedback" => Ok(Self::OrchestratorFeedback),
            "orchestrator:escalation" => Ok(Self::OrchestratorEscalation),
            "orchestrator:decision" => Ok(Self::OrchestratorDecision),
            "orchestrator:message" => Ok(Self::OrchestratorMessage),
            "orchestrator:response" => Ok(Self::OrchestratorResponse),
            "orchestrator:thought" => Ok(Self::OrchestratorThought),
            "system:started" => Ok(Self::SystemStarted),
            "system:mode:play" => Ok(Self::SystemModePlay),
            "system:mode:pause" => Ok(Self::SystemModePause),
            "system:mode:stop" => Ok(Self::SystemModeStop),
            "system:flush" => Ok(Self::SystemFlush),
            "system:config:reloaded" => Ok(Self::SystemConfigReloaded),
            "system:scheduler:tick" => Ok(Self::SystemSchedulerTick),
            "system:memory:warning" => Ok(Self::SystemMemoryWarning),
            "system:memory:pressure" => Ok(Self::SystemMemoryPressure),
            "system:memory:emergency" => Ok(Self::SystemMemoryEmergency),
            "system:time_limit:soft" => Ok(Self::SystemTimeLimitSoft),
            "system:time_limit:hard" => Ok(Self::SystemTimeLimitHard),
            "system:rebuild" => Ok(Self::SystemRebuild),
            "system:accounting:tokens" => Ok(Self::SystemAccountingTokens),
            "system:accounting:api_call" => Ok(Self::SystemAccountingApiCall),
            "system:accounting:session" => Ok(Self::SystemAccountingSession),
            "reflection:created" => Ok(Self::ReflectionCreated),
            "reflection:comment" => Ok(Self::ReflectionComment),
            "reflection:closed" => Ok(Self::ReflectionClosed),
            "system:update:available" => Ok(Self::SystemUpdateAvailable),
            "system:update:applying" => Ok(Self::SystemUpdateApplying),
            _ => Err(format!("unknown event type: {}", s)),
        }
    }
}

/// Current event schema version.
///
/// Bump this when the `Event` serialization format changes (new required
/// fields, renamed fields, type changes). Each bump must have a
/// corresponding migration arm in [`Event::migrate`].
///
/// History:
/// - **0** — Pre-versioning events (no `schema_version` field present).
/// - **1** — Added `schema_version` field. No structural changes.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Default used by serde when deserializing events that predate schema
/// versioning (i.e. they have no `schema_version` field at all).
fn default_schema_version() -> u32 {
    0
}

/// An event in the system.
///
/// Events are immutable once created. They form an append-only log
/// that provides a complete audit trail of all activity.
///
/// ## Schema versioning
///
/// The `schema_version` field tracks which schema produced the event.
/// Old logs written before versioning was added will deserialize with
/// version `0`. Use [`Event::migrate`] to upgrade an event to the
/// current schema version.
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
    /// Schema version of this event. Defaults to `0` for events written
    /// before versioning was introduced.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
}

impl Event {
    /// Create a new event with the current timestamp and current schema version.
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
            schema_version: CURRENT_SCHEMA_VERSION,
        }
    }

    /// Migrate this event to the current schema version, applying any
    /// necessary transformations in sequence.
    ///
    /// Returns `Ok(())` if the event was already current or was
    /// successfully migrated. Returns `Err` with a description if the
    /// event has an unrecognized future version.
    ///
    /// When adding a new schema version N, add a migration arm that
    /// transforms version N-1 → N (adjusting fields / data as needed)
    /// and bumps `schema_version` to N.
    pub fn migrate(&mut self) -> Result<(), String> {
        loop {
            match self.schema_version {
                // v0 → v1: Pre-versioning events. No structural changes
                // needed — just stamp the version.
                0 => {
                    self.schema_version = 1;
                }
                // Already at current version — done.
                v if v == CURRENT_SCHEMA_VERSION => return Ok(()),
                // Future version we don't know how to handle.
                v => {
                    return Err(format!(
                        "event schema version {v} is newer than supported version \
                         {CURRENT_SCHEMA_VERSION}; upgrade your binary"
                    ));
                }
            }
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
    fn new_event_has_current_schema_version() {
        let e = Event::new(
            EventType::TaskCreated,
            "task-123",
            Actor::System,
            serde_json::json!({}),
        );
        assert_eq!(e.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn deserialize_legacy_event_without_schema_version() {
        // Simulate a pre-versioning event (no schema_version field).
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "type": "task:created",
            "task": "task-1",
            "actor": "system",
            "ts": "2025-01-01T00:00:00Z",
            "data": {}
        }"#;
        let event: Event = serde_json::from_str(json).unwrap();
        assert_eq!(event.schema_version, 0);
    }

    #[test]
    fn migrate_v0_to_current() {
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "type": "task:created",
            "task": "task-1",
            "actor": "system",
            "ts": "2025-01-01T00:00:00Z",
            "data": {}
        }"#;
        let mut event: Event = serde_json::from_str(json).unwrap();
        assert_eq!(event.schema_version, 0);
        event.migrate().unwrap();
        assert_eq!(event.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn migrate_current_is_noop() {
        let mut e = Event::new(
            EventType::TaskCreated,
            "task-123",
            Actor::System,
            serde_json::json!({}),
        );
        let original_version = e.schema_version;
        e.migrate().unwrap();
        assert_eq!(e.schema_version, original_version);
    }

    #[test]
    fn migrate_future_version_returns_error() {
        let mut e = Event::new(
            EventType::TaskCreated,
            "task-123",
            Actor::System,
            serde_json::json!({}),
        );
        e.schema_version = CURRENT_SCHEMA_VERSION + 99;
        let result = e.migrate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("newer than supported"));
    }

    #[test]
    fn schema_version_included_in_serialized_json() {
        let e = Event::new(
            EventType::TaskCreated,
            "task-123",
            Actor::System,
            serde_json::json!({}),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"schema_version\":1"));
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

    #[test]
    fn time_limit_soft_event_serializes() {
        let e = Event::new(
            EventType::SystemTimeLimitSoft,
            "task-123",
            Actor::System,
            serde_json::json!({
                "elapsed_seconds": 3600,
                "soft_limit_seconds": 3600,
                "hard_limit_seconds": 4500,
            }),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"system:time_limit:soft\""));
        assert!(json.contains("\"elapsed_seconds\":3600"));
    }

    #[test]
    fn time_limit_hard_event_serializes() {
        let e = Event::new(
            EventType::SystemTimeLimitHard,
            "task-123",
            Actor::System,
            serde_json::json!({
                "elapsed_seconds": 4500,
                "hard_limit_seconds": 4500,
            }),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"system:time_limit:hard\""));
        assert!(json.contains("\"hard_limit_seconds\":4500"));
    }

    #[test]
    fn time_limit_soft_deserializes() {
        let s = "system:time_limit:soft".to_string();
        let t = EventType::try_from(s).unwrap();
        assert_eq!(t, EventType::SystemTimeLimitSoft);
    }

    #[test]
    fn time_limit_hard_deserializes() {
        let s = "system:time_limit:hard".to_string();
        let t = EventType::try_from(s).unwrap();
        assert_eq!(t, EventType::SystemTimeLimitHard);
    }

    #[test]
    fn time_limit_events_match_system_wildcard() {
        let soft = EventType::SystemTimeLimitSoft;
        let hard = EventType::SystemTimeLimitHard;
        assert!(soft.matches("system:*"));
        assert!(hard.matches("system:*"));
        assert!(soft.matches("system:time_limit:*"));
        assert!(hard.matches("system:time_limit:*"));
    }

    #[test]
    fn accounting_tokens_event_serializes() {
        let e = Event::new(
            EventType::SystemAccountingTokens,
            "task-123",
            Actor::System,
            serde_json::json!({
                "input_tokens": 1500,
                "output_tokens": 800,
                "model": "claude-opus-4-6",
            }),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"system:accounting:tokens\""));
        assert!(json.contains("\"input_tokens\":1500"));
    }

    #[test]
    fn accounting_session_event_serializes() {
        let e = Event::new(
            EventType::SystemAccountingSession,
            "task-123",
            Actor::System,
            serde_json::json!({
                "duration_seconds": 3600,
                "total_input_tokens": 15000,
                "total_output_tokens": 8000,
            }),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"system:accounting:session\""));
        assert!(json.contains("\"duration_seconds\":3600"));
    }

    #[test]
    fn accounting_events_deserialize() {
        let tokens = EventType::try_from("system:accounting:tokens".to_string()).unwrap();
        assert_eq!(tokens, EventType::SystemAccountingTokens);

        let api_call = EventType::try_from("system:accounting:api_call".to_string()).unwrap();
        assert_eq!(api_call, EventType::SystemAccountingApiCall);

        let session = EventType::try_from("system:accounting:session".to_string()).unwrap();
        assert_eq!(session, EventType::SystemAccountingSession);
    }

    #[test]
    fn accounting_events_match_system_wildcard() {
        let tokens = EventType::SystemAccountingTokens;
        let api_call = EventType::SystemAccountingApiCall;
        let session = EventType::SystemAccountingSession;

        assert!(tokens.matches("system:*"));
        assert!(api_call.matches("system:*"));
        assert!(session.matches("system:*"));

        assert!(tokens.matches("system:accounting:*"));
        assert!(api_call.matches("system:accounting:*"));
        assert!(session.matches("system:accounting:*"));
    }

    #[test]
    fn update_available_event_serializes() {
        let e = Event::new(
            EventType::SystemUpdateAvailable,
            "",
            Actor::System,
            serde_json::json!({
                "target_commit": "def5678",
                "rebuild_scope": "server",
                "commit_summary": "Fix SSE presence guard (#279)",
            }),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"system:update:available\""));
        assert!(json.contains("\"target_commit\":\"def5678\""));
    }

    #[test]
    fn update_applying_event_serializes() {
        let e = Event::new(
            EventType::SystemUpdateApplying,
            "",
            Actor::System,
            serde_json::json!({
                "target_commit": "def5678",
                "sessions_remaining": 2,
            }),
        );
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"system:update:applying\""));
        assert!(json.contains("\"sessions_remaining\":2"));
    }

    #[test]
    fn update_events_deserialize() {
        let available = EventType::try_from("system:update:available".to_string()).unwrap();
        assert_eq!(available, EventType::SystemUpdateAvailable);

        let applying = EventType::try_from("system:update:applying".to_string()).unwrap();
        assert_eq!(applying, EventType::SystemUpdateApplying);
    }

    #[test]
    fn update_events_match_system_wildcard() {
        let available = EventType::SystemUpdateAvailable;
        let applying = EventType::SystemUpdateApplying;

        assert!(available.matches("system:*"));
        assert!(applying.matches("system:*"));
    }

    #[test]
    fn update_events_match_update_wildcard() {
        let available = EventType::SystemUpdateAvailable;
        let applying = EventType::SystemUpdateApplying;

        assert!(available.matches("system:update:*"));
        assert!(applying.matches("system:update:*"));
        assert!(!EventType::SystemModePlay.matches("system:update:*"));
    }
}
