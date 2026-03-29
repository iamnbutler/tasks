import type { TaskState } from "./types";

/**
 * Valid state transitions mirroring the server's TaskState::can_transition_to().
 * See crates/models/src/task.rs for the authoritative source.
 */
const TRANSITIONS: Record<TaskState, TaskState[]> = {
  waiting: ["running", "blocked", "cancelled"],
  blocked: ["waiting", "cancelled"],
  running: [
    "question",
    "testing",
    "awaiting_merge",
    "conflict",
    "changes_requested",
    "completed",
    "failed",
    "cancelled",
    "waiting",
  ],
  question: ["running", "waiting", "failed", "cancelled"],
  testing: ["running", "awaiting_merge", "waiting", "failed", "cancelled"],
  awaiting_merge: [
    "completed",
    "conflict",
    "changes_requested",
    "failed",
    "cancelled",
  ],
  conflict: ["running", "changes_requested", "failed", "cancelled"],
  changes_requested: ["running", "waiting", "failed", "cancelled"],
  failed: ["waiting"],
  completed: [],
  cancelled: [],
};

/** Get the list of states a task can transition to from its current state. */
export function allowedTransitions(state: TaskState): TaskState[] {
  return TRANSITIONS[state] ?? [];
}

/** Check whether a specific transition is valid. */
export function canTransitionTo(
  from: TaskState,
  to: TaskState
): boolean {
  return TRANSITIONS[from]?.includes(to) ?? false;
}

/** Whether a state is terminal (no outbound transitions). */
export function isTerminalState(state: TaskState): boolean {
  return (TRANSITIONS[state]?.length ?? 0) === 0;
}
