//! Token and cost accounting types (spec §16.4).
//!
//! Tracks resource consumption per task and per project:
//! - Agent tokens (input/output)
//! - API calls (GitHub, etc.)
//! - Session duration

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Token usage from an agent interaction.
///
/// Represents a snapshot of cumulative token usage from an agent session.
/// Per spec §13.5, we prefer absolute totals over deltas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Total input tokens consumed.
    pub input_tokens: u64,
    /// Total output tokens consumed.
    pub output_tokens: u64,
    /// Model identifier (e.g., "claude-3-opus").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl TokenUsage {
    /// Create a new TokenUsage record.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            model: None,
        }
    }

    /// Create a new TokenUsage record with a model identifier.
    pub fn with_model(input_tokens: u64, output_tokens: u64, model: impl Into<String>) -> Self {
        Self {
            input_tokens,
            output_tokens,
            model: Some(model.into()),
        }
    }

    /// Total tokens (input + output).
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Calculate the delta between this usage and a previous usage.
    ///
    /// Used to compute incremental token counts when we receive
    /// cumulative totals from the agent.
    pub fn delta(&self, previous: &TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            model: self.model.clone(),
        }
    }

    /// Add another usage to this one.
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// API call accounting record.
///
/// Tracks calls to external services like GitHub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCallRecord {
    /// Service name (e.g., "github").
    pub service: String,
    /// Endpoint or operation (e.g., "graphql", "rest/issues").
    pub endpoint: String,
    /// Rate limit points consumed (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points_consumed: Option<u32>,
    /// Rate limit remaining after this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_remaining: Option<u32>,
    /// When the call was made.
    pub timestamp: DateTime<Utc>,
}

impl ApiCallRecord {
    /// Create a new API call record.
    pub fn new(service: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            endpoint: endpoint.into(),
            points_consumed: None,
            rate_limit_remaining: None,
            timestamp: Utc::now(),
        }
    }

    /// Set the rate limit info.
    pub fn with_rate_limit(mut self, consumed: u32, remaining: u32) -> Self {
        self.points_consumed = Some(consumed);
        self.rate_limit_remaining = Some(remaining);
        self
    }
}

/// Session duration record.
///
/// Tracks how long a session ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDurationRecord {
    /// Task ID this session was for.
    pub task_id: String,
    /// Session start time.
    pub started_at: DateTime<Utc>,
    /// Session end time.
    pub ended_at: DateTime<Utc>,
    /// Duration in seconds.
    pub duration_seconds: u64,
}

impl SessionDurationRecord {
    /// Create a new session duration record.
    pub fn new(
        task_id: impl Into<String>,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
    ) -> Self {
        let duration_seconds = (ended_at - started_at).num_seconds().max(0) as u64;
        Self {
            task_id: task_id.into(),
            started_at,
            ended_at,
            duration_seconds,
        }
    }
}

/// Aggregated accounting summary for a task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskAccountingSummary {
    /// Task ID.
    pub task_id: String,
    /// Total token usage across all sessions.
    pub tokens: TokenUsage,
    /// Total session duration in seconds.
    pub total_duration_seconds: u64,
    /// Number of sessions run.
    pub session_count: u32,
}

impl TaskAccountingSummary {
    /// Create a new task accounting summary.
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            ..Default::default()
        }
    }

    /// Add token usage.
    pub fn add_tokens(&mut self, usage: &TokenUsage) {
        self.tokens.add(usage);
    }

    /// Add session duration.
    pub fn add_session(&mut self, duration_seconds: u64) {
        self.total_duration_seconds += duration_seconds;
        self.session_count += 1;
    }
}

/// Aggregated accounting summary for the entire system.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalAccountingSummary {
    /// Total token usage.
    pub tokens: TokenUsage,
    /// Total session duration in seconds.
    pub total_duration_seconds: u64,
    /// Total number of sessions.
    pub session_count: u32,
    /// Total API calls.
    pub api_call_count: u32,
}

impl GlobalAccountingSummary {
    /// Add token usage.
    pub fn add_tokens(&mut self, usage: &TokenUsage) {
        self.tokens.add(usage);
    }

    /// Add session duration.
    pub fn add_session(&mut self, duration_seconds: u64) {
        self.total_duration_seconds += duration_seconds;
        self.session_count += 1;
    }

    /// Increment API call count.
    pub fn add_api_call(&mut self) {
        self.api_call_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_total() {
        let usage = TokenUsage::new(1000, 500);
        assert_eq!(usage.total_tokens(), 1500);
    }

    #[test]
    fn token_usage_delta() {
        let previous = TokenUsage::new(1000, 500);
        let current = TokenUsage::new(1500, 800);
        let delta = current.delta(&previous);
        assert_eq!(delta.input_tokens, 500);
        assert_eq!(delta.output_tokens, 300);
    }

    #[test]
    fn token_usage_delta_saturates() {
        let previous = TokenUsage::new(1000, 500);
        let current = TokenUsage::new(500, 300);
        let delta = current.delta(&previous);
        assert_eq!(delta.input_tokens, 0);
        assert_eq!(delta.output_tokens, 0);
    }

    #[test]
    fn token_usage_add() {
        let mut total = TokenUsage::new(1000, 500);
        total.add(&TokenUsage::new(200, 100));
        assert_eq!(total.input_tokens, 1200);
        assert_eq!(total.output_tokens, 600);
    }

    #[test]
    fn token_usage_serializes() {
        let usage = TokenUsage::with_model(1500, 800, "claude-3-opus");
        let json = serde_json::to_string(&usage).unwrap();
        assert!(json.contains("\"input_tokens\":1500"));
        assert!(json.contains("\"output_tokens\":800"));
        assert!(json.contains("\"model\":\"claude-3-opus\""));
    }

    #[test]
    fn token_usage_without_model_serializes() {
        let usage = TokenUsage::new(1500, 800);
        let json = serde_json::to_string(&usage).unwrap();
        assert!(!json.contains("model"));
    }

    #[test]
    fn api_call_record_new() {
        let record = ApiCallRecord::new("github", "graphql")
            .with_rate_limit(1, 4999);
        assert_eq!(record.service, "github");
        assert_eq!(record.endpoint, "graphql");
        assert_eq!(record.points_consumed, Some(1));
        assert_eq!(record.rate_limit_remaining, Some(4999));
    }

    #[test]
    fn session_duration_calculates() {
        let start = DateTime::parse_from_rfc3339("2024-01-01T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2024-01-01T11:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let record = SessionDurationRecord::new("task-1", start, end);
        assert_eq!(record.duration_seconds, 5400); // 1.5 hours
    }

    #[test]
    fn task_accounting_summary() {
        let mut summary = TaskAccountingSummary::new("task-1");
        summary.add_tokens(&TokenUsage::new(1000, 500));
        summary.add_tokens(&TokenUsage::new(500, 250));
        summary.add_session(3600);
        summary.add_session(1800);

        assert_eq!(summary.tokens.input_tokens, 1500);
        assert_eq!(summary.tokens.output_tokens, 750);
        assert_eq!(summary.total_duration_seconds, 5400);
        assert_eq!(summary.session_count, 2);
    }

    #[test]
    fn global_accounting_summary() {
        let mut summary = GlobalAccountingSummary::default();
        summary.add_tokens(&TokenUsage::new(1000, 500));
        summary.add_session(3600);
        summary.add_api_call();
        summary.add_api_call();

        assert_eq!(summary.tokens.total_tokens(), 1500);
        assert_eq!(summary.total_duration_seconds, 3600);
        assert_eq!(summary.session_count, 1);
        assert_eq!(summary.api_call_count, 2);
    }
}
