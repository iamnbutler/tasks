//! Agent output interpreter — spec §9.3.
//!
//! Interprets agent output text to infer state transitions and emit appropriate events.
//! The agent itself does not know about the Tasks event system; this module observes
//! agent behavior and infers what's happening.
//!
//! Design principle: False positives are worse than false negatives. Start conservative
//! with high-confidence patterns.

use events::{Actor, Event, EventBus, EventType};

/// Result of interpreting agent output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSignal {
    /// No special signal detected — just a normal message.
    Message,
    /// Agent is asking a question and waiting for input.
    Question { text: String },
    /// Agent indicates task completion.
    Completion { text: String },
    /// Agent indicates failure or being stuck.
    Failure { text: String },
}

/// Interpreter for agent output text.
///
/// Analyzes agent stdout to detect questions, completion signals, and failure patterns.
/// Emits appropriate events based on detected patterns.
pub struct OutputInterpreter {
    /// Recent output lines for context-aware detection.
    recent_lines: Vec<String>,
    /// Maximum number of recent lines to keep.
    max_recent_lines: usize,
}

impl Default for OutputInterpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputInterpreter {
    /// Create a new output interpreter.
    pub fn new() -> Self {
        Self {
            recent_lines: Vec::new(),
            max_recent_lines: 20,
        }
    }

    /// Set the maximum number of recent lines to track.
    pub fn with_max_recent_lines(mut self, max: usize) -> Self {
        self.max_recent_lines = max;
        self
    }

    /// Analyze a line of agent output and return the detected signal.
    pub fn interpret(&mut self, text: &str) -> OutputSignal {
        // Store for context
        self.recent_lines.push(text.to_string());
        if self.recent_lines.len() > self.max_recent_lines {
            self.recent_lines.remove(0);
        }

        let trimmed = text.trim();

        // Note: Question detection is disabled (see #415).
        //
        // The output-based question detection was producing too many false positives.
        // Agents frequently use question-like language ("should I...", "please provide...")
        // while explaining their work, not when actually blocked waiting for input.
        // Since agents don't have an explicit protocol to signal "I'm blocked", and they
        // continue working even after outputting question-like text, the pattern matching
        // approach is fundamentally unreliable.
        //
        // The Question state infrastructure is preserved for potential future use
        // (e.g., if agents gain an explicit "waiting for input" signal).

        // Check for failure patterns
        if let Some(signal) = self.detect_failure(trimmed) {
            return signal;
        }

        // Check for completion patterns
        if let Some(signal) = self.detect_completion(trimmed) {
            return signal;
        }

        OutputSignal::Message
    }

    /// Detect question patterns in output.
    ///
    /// NOTE: This method is currently unused (see #415). The output-based question
    /// detection was producing too many false positives because agents use question-like
    /// language while working, not when actually blocked waiting for input.
    ///
    /// The method is preserved in case a more reliable detection mechanism is developed
    /// (e.g., combined with agent pause signals or tool calls).
    ///
    /// Original design: High-confidence patterns that indicate the agent is waiting for human input:
    /// - Explicit prompts asking for input
    /// - Multiple choice questions
    /// - Permission requests
    #[allow(dead_code)]
    fn detect_question(&self, text: &str) -> Option<OutputSignal> {
        let lower = text.to_lowercase();

        // High-confidence: Explicit input prompts
        // These are strong indicators the agent is blocked waiting for input
        let explicit_prompts = [
            "please provide",
            "please specify",
            "please confirm",
            "please choose",
            "please select",
            "which option",
            "which approach",
            "which one",
            "would you like me to",
            "should i proceed",
            "should i continue",
            "do you want me to",
            "can you provide",
            "can you clarify",
            "can you confirm",
            "i need clarification",
            "i need more information",
            "awaiting your input",
            "waiting for your response",
            "waiting for your input",
        ];

        for prompt in explicit_prompts {
            if lower.contains(prompt) {
                return Some(OutputSignal::Question {
                    text: text.to_string(),
                });
            }
        }

        // High-confidence: Questions with specific patterns
        // Must end with ? AND contain question indicators to reduce false positives
        if text.ends_with('?') {
            // Questions that are inherently directed at the user (asking permission/preference)
            let user_directed_starters = [
                "should i ",
                "shall i ",
                "would you like",
                "do you want",
                "can i ",
                "may i ",
            ];

            for starter in user_directed_starters {
                if lower.starts_with(starter) {
                    return Some(OutputSignal::Question {
                        text: text.to_string(),
                    });
                }
            }

            // Questions that need "you/your" to confirm they're directed at the user
            let question_starters = [
                "what ",
                "which ",
                "how ",
                "where ",
                "when ",
                "why ",
                "would ",
                "should ",
                "could ",
                "can ",
                "do you ",
                "does ",
                "is this ",
                "are you ",
            ];

            for starter in question_starters {
                if lower.starts_with(starter) || lower.contains(&format!(" {}", starter.trim())) {
                    // Only treat as a blocking question if it seems to be asking the user
                    // Check for second-person pronouns to indicate it's directed at the user
                    if lower.contains(" you ") || lower.contains(" your ") || lower.ends_with(" you?")
                    {
                        return Some(OutputSignal::Question {
                            text: text.to_string(),
                        });
                    }
                }
            }
        }

        None
    }

    /// Detect failure patterns in output.
    ///
    /// High-confidence patterns that indicate the agent has hit a wall:
    /// - Explicit stuck/blocked statements
    /// - Inability to proceed
    /// - Repeated errors (context-aware)
    fn detect_failure(&self, text: &str) -> Option<OutputSignal> {
        let lower = text.to_lowercase();

        // High-confidence: Explicit failure/stuck statements
        let failure_patterns = [
            "i am stuck",
            "i'm stuck",
            "i cannot proceed",
            "i can't proceed",
            "i am unable to",
            "i'm unable to",
            "unable to continue",
            "cannot continue",
            "can't continue",
            "i've hit a wall",
            "i have hit a wall",
            "this is beyond my capabilities",
            "i need help to proceed",
            "i cannot resolve this",
            "i can't resolve this",
            "i don't know how to proceed",
            "i'm not sure how to proceed",
            "this task cannot be completed",
            "this task can't be completed",
            "i have failed to",
            "i've failed to",
        ];

        for pattern in failure_patterns {
            if lower.contains(pattern) {
                return Some(OutputSignal::Failure {
                    text: text.to_string(),
                });
            }
        }

        // Context-aware: Check for repeated error patterns in recent lines
        // If we see multiple "error:" or "failed to" in recent output, it might indicate
        // the agent is in a failure loop
        if self.detect_error_loop() {
            // Only signal failure if the current line also mentions error/failure
            if lower.contains("error") || lower.contains("failed") || lower.contains("failure") {
                return Some(OutputSignal::Failure {
                    text: "Repeated errors detected".to_string(),
                });
            }
        }

        None
    }

    /// Check if there's a pattern of repeated errors in recent lines.
    fn detect_error_loop(&self) -> bool {
        if self.recent_lines.len() < 5 {
            return false;
        }

        let error_patterns = ["error:", "error!", "failed to", "failure:"];
        let error_count = self
            .recent_lines
            .iter()
            .rev()
            .take(10)
            .filter(|line| {
                let lower = line.to_lowercase();
                error_patterns.iter().any(|p| lower.contains(p))
            })
            .count();

        // If more than 3 error-like lines in the last 10, consider it a potential loop
        error_count >= 3
    }

    /// Detect completion patterns in output.
    ///
    /// High-confidence patterns that indicate the agent believes it's done:
    /// - Explicit completion statements
    /// - PR creation confirmations
    /// - Commit/push confirmations
    fn detect_completion(&self, text: &str) -> Option<OutputSignal> {
        let lower = text.to_lowercase();

        // High-confidence: Explicit completion statements
        let completion_patterns = [
            "task completed",
            "task complete",
            "task is complete",
            "task has been completed",
            "all changes committed",
            "changes have been committed",
            "changes have been pushed",
            "i have completed the task",
            "i've completed the task",
            "the task is finished",
            "the task is done",
            "work is complete",
            "implementation complete",
            "implementation is complete",
            "all done",
            "finished implementing",
            "successfully completed",
        ];

        for pattern in completion_patterns {
            if lower.contains(pattern) {
                return Some(OutputSignal::Completion {
                    text: text.to_string(),
                });
            }
        }

        // High-confidence: PR creation confirmations
        // These indicate the agent has created a PR which signals completion
        let pr_patterns = [
            "created pull request",
            "opened pull request",
            "pr has been created",
            "pull request created",
            "created pr #",
            "opened pr #",
        ];

        for pattern in pr_patterns {
            if lower.contains(pattern) {
                return Some(OutputSignal::Completion {
                    text: text.to_string(),
                });
            }
        }

        None
    }

    /// Clear the recent lines buffer (e.g., on session restart).
    pub fn clear(&mut self) {
        self.recent_lines.clear();
    }
}

/// Emit events based on the interpreted signal.
///
/// This is a helper function that emits the appropriate events to the event bus
/// based on the detected signal. It handles the logic for which events to emit
/// for each signal type.
pub async fn emit_signal_events(
    task_id: &str,
    signal: &OutputSignal,
    event_bus: &EventBus,
) -> Result<(), events::StoreError> {
    match signal {
        OutputSignal::Question { text } => {
            // Emit agent:question event
            let question_event = Event::new(
                EventType::AgentQuestion,
                task_id,
                Actor::Agent,
                serde_json::json!({
                    "text": text,
                    "source": "output_interpretation",
                }),
            );
            event_bus.publish(question_event).await?;

            // Emit task:state:question to trigger state transition
            let state_event = Event::new(
                EventType::TaskStateQuestion,
                task_id,
                Actor::Agent,
                serde_json::json!({
                    "reason": "agent_question",
                    "question": text,
                }),
            );
            event_bus.publish(state_event).await?;
        }
        OutputSignal::Failure { text } => {
            // Emit agent:error event
            let error_event = Event::new(
                EventType::AgentError,
                task_id,
                Actor::Agent,
                serde_json::json!({
                    "text": text,
                    "source": "output_interpretation",
                }),
            );
            event_bus.publish(error_event).await?;

            // Note: We don't emit task:state:failed here because the agent hasn't
            // actually exited yet. The orchestrator or session manager should
            // decide whether to transition to failed state based on this error.
        }
        OutputSignal::Completion { text } => {
            // Emit an orchestrator-informative event
            // Note: We don't automatically transition to awaiting_merge because
            // the agent exit handler should still run and verify the exit code.
            // This is informational for the orchestrator.
            let completion_hint_event = Event::new(
                EventType::AgentMessage,
                task_id,
                Actor::Agent,
                serde_json::json!({
                    "text": text,
                    "completion_hint": true,
                    "source": "output_interpretation",
                }),
            );
            event_bus.publish(completion_hint_event).await?;
        }
        OutputSignal::Message => {
            // No special events for regular messages
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_patterns_not_detected() {
        // Question detection is disabled (see #415) because output-based pattern
        // matching produced too many false positives. Agents use question-like
        // language while working, not when actually blocked waiting for input.
        let mut interpreter = OutputInterpreter::new();

        let question_like_text = [
            "Please provide the path to the config file.",
            "Should I proceed with the refactoring?",
            "What would you like me to do next?",
            "Can you provide more context about the bug?",
        ];

        for q in question_like_text {
            let signal = interpreter.interpret(q);
            assert!(
                matches!(signal, OutputSignal::Message),
                "Question detection is disabled, should return Message: {}",
                q
            );
        }
    }

    #[test]
    fn detect_failure_patterns() {
        let mut interpreter = OutputInterpreter::new();

        let failures = [
            "I'm stuck and cannot proceed with this task.",
            "I cannot proceed without additional information.",
            "I'm unable to resolve this merge conflict.",
            "Unable to continue due to missing dependencies.",
            "I've hit a wall with this approach.",
            "This is beyond my capabilities.",
        ];

        for f in failures {
            let signal = interpreter.interpret(f);
            assert!(
                matches!(signal, OutputSignal::Failure { .. }),
                "Should detect failure: {}",
                f
            );
        }
    }

    #[test]
    fn detect_completion_patterns() {
        let mut interpreter = OutputInterpreter::new();

        let completions = [
            "Task completed successfully.",
            "All changes committed and pushed.",
            "I have completed the task as requested.",
            "The task is finished.",
            "Implementation complete.",
            "Created pull request #42.",
            "PR has been created and is ready for review.",
        ];

        for c in completions {
            let signal = interpreter.interpret(c);
            assert!(
                matches!(signal, OutputSignal::Completion { .. }),
                "Should detect completion: {}",
                c
            );
        }
    }

    #[test]
    fn normal_messages_not_detected() {
        let mut interpreter = OutputInterpreter::new();

        let normal = [
            "Reading the file...",
            "I'm analyzing the code structure.",
            "The function appears to handle user input.",
            "Let me check the documentation.",
            "Running the tests now.",
            "Building the project...",
        ];

        for n in normal {
            let signal = interpreter.interpret(n);
            assert!(
                matches!(signal, OutputSignal::Message),
                "Should not detect signal: {}",
                n
            );
        }
    }

    #[test]
    fn error_loop_detection() {
        let mut interpreter = OutputInterpreter::new();

        // Simulate a series of errors
        interpreter.interpret("Running test...");
        interpreter.interpret("Error: assertion failed");
        interpreter.interpret("Retrying...");
        interpreter.interpret("Error: assertion failed");
        interpreter.interpret("Trying different approach...");
        interpreter.interpret("Error: still failing");
        interpreter.interpret("Error: cannot resolve");

        // The next error line should trigger failure detection
        let signal = interpreter.interpret("Error: giving up");
        assert!(matches!(signal, OutputSignal::Failure { .. }));
    }

    #[test]
    fn clear_resets_context() {
        let mut interpreter = OutputInterpreter::new();

        // Add some lines
        interpreter.interpret("Error: test 1");
        interpreter.interpret("Error: test 2");
        interpreter.interpret("Error: test 3");
        interpreter.interpret("Error: test 4");
        interpreter.interpret("Error: test 5");

        // Clear should reset
        interpreter.clear();

        // Now an error line should not trigger error loop detection
        let signal = interpreter.interpret("Error: single error");
        assert!(matches!(signal, OutputSignal::Message));
    }

}
