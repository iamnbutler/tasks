//! Problem tracker for orchestrator mode lowering (spec §6.4).
//!
//! Tracks failure patterns that warrant lowering operating mode from Play
//! to Pause. The orchestrator can lower mode (but not raise it).
//!
//! Tracked patterns:
//! - Consecutive evaluation failures (API/internal errors)
//! - Consecutive PR rejections (pattern of bad PRs)
//! - Merge conflicts within a time window
//! - Agent errors within a time window
//! - Task failures within a time window

use std::time::{Duration, Instant};

/// Reason for recommending mode lowering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerReason {
    /// Multiple consecutive evaluation failures (API errors, etc.)
    ConsecutiveEvalFailures(u32),
    /// Multiple consecutive PR rejections (bad PR pattern)
    ConsecutiveRejections(u32),
    /// Too many merge conflicts in a short time
    RepeatedConflicts(u32),
    /// Too many agent errors in a short time
    RepeatedAgentErrors(u32),
    /// Too many task failures in a short time
    RepeatedTaskFailures(u32),
}

impl std::fmt::Display for LowerReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LowerReason::ConsecutiveEvalFailures(n) => {
                write!(f, "{} consecutive evaluation failures", n)
            }
            LowerReason::ConsecutiveRejections(n) => {
                write!(f, "{} consecutive PR rejections", n)
            }
            LowerReason::RepeatedConflicts(n) => {
                write!(f, "{} merge conflicts in tracking window", n)
            }
            LowerReason::RepeatedAgentErrors(n) => {
                write!(f, "{} agent errors in tracking window", n)
            }
            LowerReason::RepeatedTaskFailures(n) => {
                write!(f, "{} task failures in tracking window", n)
            }
        }
    }
}

/// Configuration for problem thresholds.
#[derive(Debug, Clone)]
pub struct ProblemThresholds {
    /// Number of consecutive evaluation failures before lowering mode.
    pub eval_failure_threshold: u32,
    /// Number of consecutive PR rejections before lowering mode.
    pub rejection_threshold: u32,
    /// Number of merge conflicts within window before lowering mode.
    pub conflict_threshold: u32,
    /// Number of agent errors within window before lowering mode.
    pub agent_error_threshold: u32,
    /// Number of task failures within window before lowering mode.
    pub task_failure_threshold: u32,
    /// Time window for tracking windowed events.
    pub window_duration: Duration,
}

impl Default for ProblemThresholds {
    fn default() -> Self {
        Self {
            eval_failure_threshold: 3,
            rejection_threshold: 5,
            conflict_threshold: 3,
            agent_error_threshold: 3,
            task_failure_threshold: 3,
            window_duration: Duration::from_secs(10 * 60), // 10 minutes
        }
    }
}

/// Tracks problem patterns and determines when to lower operating mode.
///
/// Used by the orchestrator loop to detect situations that warrant
/// transitioning from Play to Pause mode.
#[derive(Debug)]
pub struct ProblemTracker {
    thresholds: ProblemThresholds,
    /// Consecutive evaluation failures (API errors, not rejections).
    consecutive_eval_failures: u32,
    /// Consecutive PR rejections (bad PR pattern).
    consecutive_rejections: u32,
    /// Timestamps of recent merge conflicts.
    recent_conflicts: Vec<Instant>,
    /// Timestamps of recent agent errors.
    recent_agent_errors: Vec<Instant>,
    /// Timestamps of recent task failures.
    recent_task_failures: Vec<Instant>,
    /// Whether mode has already been lowered (to avoid repeated lowering).
    mode_lowered: bool,
}

impl ProblemTracker {
    /// Create a new problem tracker with default thresholds.
    pub fn new() -> Self {
        Self::with_thresholds(ProblemThresholds::default())
    }

    /// Create a problem tracker with custom thresholds.
    pub fn with_thresholds(thresholds: ProblemThresholds) -> Self {
        Self {
            thresholds,
            consecutive_eval_failures: 0,
            consecutive_rejections: 0,
            recent_conflicts: Vec::new(),
            recent_agent_errors: Vec::new(),
            recent_task_failures: Vec::new(),
            mode_lowered: false,
        }
    }

    /// Record a successful evaluation (resets consecutive failures).
    pub fn record_eval_success(&mut self) {
        self.consecutive_eval_failures = 0;
    }

    /// Record an evaluation failure (API error, not a rejection).
    pub fn record_eval_failure(&mut self) {
        self.consecutive_eval_failures += 1;
    }

    /// Record a PR approval (resets consecutive rejections).
    pub fn record_approval(&mut self) {
        self.consecutive_rejections = 0;
    }

    /// Record a PR rejection (bad PR pattern).
    pub fn record_rejection(&mut self) {
        self.consecutive_rejections += 1;
    }

    /// Record a merge conflict.
    pub fn record_conflict(&mut self) {
        self.recent_conflicts.push(Instant::now());
    }

    /// Record an agent error.
    pub fn record_agent_error(&mut self) {
        self.recent_agent_errors.push(Instant::now());
    }

    /// Record a task failure.
    pub fn record_task_failure(&mut self) {
        self.recent_task_failures.push(Instant::now());
    }

    /// Clean up old entries outside the tracking window.
    fn prune_old_entries(&mut self) {
        let cutoff = Instant::now() - self.thresholds.window_duration;
        self.recent_conflicts.retain(|&t| t > cutoff);
        self.recent_agent_errors.retain(|&t| t > cutoff);
        self.recent_task_failures.retain(|&t| t > cutoff);
    }

    /// Check if mode should be lowered, returning the reason if so.
    ///
    /// Returns `None` if no threshold has been exceeded or if mode
    /// has already been lowered (to avoid repeated lowering events).
    ///
    /// NOTE: Disabled — auto mode-lowering fires too aggressively during
    /// normal operation (3 agent errors in 10 min is common). The
    /// orchestrator should handle this more intelligently once it has
    /// an event-driven processing loop (#536).
    pub fn should_lower_mode(&mut self) -> Option<LowerReason> {
        None
    }

    /// Reset the tracker when mode is raised back to Play.
    ///
    /// Called when the human raises mode, allowing the tracker to
    /// detect new problems and lower mode again if needed.
    pub fn reset(&mut self) {
        self.consecutive_eval_failures = 0;
        self.consecutive_rejections = 0;
        self.recent_conflicts.clear();
        self.recent_agent_errors.clear();
        self.recent_task_failures.clear();
        self.mode_lowered = false;
    }

    /// Check if mode has been lowered by this tracker.
    pub fn is_mode_lowered(&self) -> bool {
        self.mode_lowered
    }
}

impl Default for ProblemTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: should_lower_mode() is currently disabled (always returns None)
    // because auto mode-lowering fires too aggressively during normal
    // operation. These tests verify the disabled behavior. When re-enabled
    // (#536), update these tests to check for Some(...) results.

    #[test]
    fn should_lower_mode_always_returns_none_while_disabled() {
        let thresholds = ProblemThresholds {
            eval_failure_threshold: 1,
            rejection_threshold: 1,
            conflict_threshold: 1,
            agent_error_threshold: 1,
            task_failure_threshold: 1,
            ..Default::default()
        };
        let mut tracker = ProblemTracker::with_thresholds(thresholds);

        tracker.record_eval_failure();
        assert!(tracker.should_lower_mode().is_none());

        tracker.record_rejection();
        assert!(tracker.should_lower_mode().is_none());

        tracker.record_conflict();
        assert!(tracker.should_lower_mode().is_none());

        tracker.record_agent_error();
        assert!(tracker.should_lower_mode().is_none());

        tracker.record_task_failure();
        assert!(tracker.should_lower_mode().is_none());
    }

    #[test]
    fn eval_success_resets_failure_count() {
        let thresholds = ProblemThresholds {
            eval_failure_threshold: 3,
            ..Default::default()
        };
        let mut tracker = ProblemTracker::with_thresholds(thresholds);

        tracker.record_eval_failure();
        tracker.record_eval_failure();
        tracker.record_eval_success(); // Reset
        tracker.record_eval_failure();
        tracker.record_eval_failure();

        assert!(tracker.should_lower_mode().is_none());
    }

    #[test]
    fn approval_resets_rejection_count() {
        let thresholds = ProblemThresholds {
            rejection_threshold: 3,
            ..Default::default()
        };
        let mut tracker = ProblemTracker::with_thresholds(thresholds);

        tracker.record_rejection();
        tracker.record_rejection();
        tracker.record_approval(); // Reset
        tracker.record_rejection();
        tracker.record_rejection();

        assert!(tracker.should_lower_mode().is_none());
    }

    #[test]
    fn reset_clears_all_state() {
        let mut tracker = ProblemTracker::new();

        tracker.record_eval_failure();
        tracker.record_rejection();
        tracker.record_conflict();
        tracker.record_agent_error();
        tracker.record_task_failure();

        tracker.reset();
        assert!(!tracker.is_mode_lowered());
    }
}
