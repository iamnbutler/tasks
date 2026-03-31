export type Mode = "stop" | "pause" | "play";

export type TaskState =
  | "waiting"
  | "blocked"
  | "running"
  | "question"
  | "testing"
  | "awaiting_merge"
  | "conflict"
  | "changes_requested"
  | "completed"
  | "failed"
  | "cancelled";

export type TaskSource =
  | { type: "github_issue"; owner: string; repo: string; number: number }
  | { type: "github_pr"; owner: string; repo: string; number: number }
  | { type: "internal"; description: string };

/** Failure classification per spec §13.1 */
export type FailureType = "transient" | "deterministic";

/** Detailed failure information per spec §13.4 */
export interface FailureInfo {
  exit_code: number | null;
  signal: string | null;
  duration_secs: number;
  stderr_tail: string[];
  failure_type: FailureType;
  summary: string;
}

export interface Task {
  id: string;
  source: TaskSource;
  title: string;
  description: string | null;
  state: TaskState;
  parent_id: string | null;
  blocked_by: string[];
  project: string;
  labels: string[];
  priority: number | null;
  session_id: string | null;
  workspace_id: string | null;
  retry_count: number;
  last_failure_at: string | null;
  last_failure: FailureInfo | null;
  rejection_feedback: string | null;
  created_at: string;
  updated_at: string;
}

export interface Project {
  id: string;
  repo: string;
  default_branch: string;
  config: Record<string, unknown>;
}

export type MergeStatus =
  | "pending"
  | "approved"
  | "merging"
  | "rejected"
  | "merged"
  | "conflict"
  | "changes_requested";

export interface MergeQueueEntry {
  id: string;
  task_id: string;
  pr_url: string;
  status: MergeStatus;
  queued_at: string;
  /** Feedback when status is changes_requested */
  changes_requested_feedback?: string;
  /** Position in merge queue (1-indexed). Only set for approved/merging entries. */
  queue_position?: number;
}

export type Actor = "human" | "orchestrator" | "scheduler" | "agent" | "system";

// ---------------------------------------------------------------------------
// Typed event payloads
// ---------------------------------------------------------------------------

/** orchestrator:decision */
export interface OrchestratorDecisionData {
  entry_id?: string;
  approved: boolean;
  reasoning?: string;
  task_id?: string;
}

/** orchestrator:feedback */
export interface OrchestratorFeedbackData {
  feedback?: string;
  context?: string;
  task_id?: string;
  // Conflict-resolution variant
  action?: string;
  entry_id?: string;
  pr_url?: string;
  success?: boolean;
}

/** orchestrator:escalation */
export interface OrchestratorEscalationData {
  action?: string;
  reasoning?: string;
  reason?: string;
  message?: string;
  entry_id?: string;
  pr_url?: string;
  from?: string;
  to?: string;
  details?: Record<string, unknown>;
}

/** orchestrator:message / orchestrator:response / orchestrator:thought */
export interface OrchestratorMessageData {
  message?: string;
  error?: boolean;
}

/** human:message */
export interface HumanMessageData {
  message?: string;
  source?: string;
}

/** agent:message */
export interface AgentMessageData {
  text?: string;
  stream?: string;
  completion_hint?: boolean;
  source?: string;
}

/** agent:error */
export interface AgentErrorData {
  text?: string;
  source?: string;
}

/** agent:question */
export interface AgentQuestionData {
  question?: string;
  message?: string;
  text?: string;
  source?: string;
}

/** automation:run:output */
export interface AutomationRunOutputData {
  automation_id?: string;
  chunk?: string;
}

/** Content block inside a parsed agent message (JSON in agent:message text) */
export interface AgentContentBlockText {
  type: "text";
  text: string;
}

export interface AgentToolInput {
  file_path?: string;
  filePath?: string;
  path?: string;
  pattern?: string;
  command?: string;
  description?: string;
  [key: string]: unknown;
}

export interface AgentContentBlockToolUse {
  type: "tool_use";
  name?: string;
  input?: AgentToolInput;
}

export interface AgentContentBlockToolResult {
  type: "tool_result";
  content?: string;
}

export interface AgentContentBlockThinking {
  type: "thinking";
}

export type AgentContentBlock =
  | AgentContentBlockText
  | AgentContentBlockToolUse
  | AgentContentBlockToolResult
  | AgentContentBlockThinking;

/** Parsed JSON message from agent stdout (the text field parsed as JSON) */
export interface AgentParsedMessage {
  type?: string;
  result?: { text?: string };
  message?: { content?: AgentContentBlock[] };
  content?: AgentContentBlock[];
}

// ---------------------------------------------------------------------------
// Event type map — maps event.type string to its data shape
// ---------------------------------------------------------------------------

export interface EventDataMap {
  "orchestrator:decision": OrchestratorDecisionData;
  "orchestrator:feedback": OrchestratorFeedbackData;
  "orchestrator:escalation": OrchestratorEscalationData;
  "orchestrator:message": OrchestratorMessageData;
  "orchestrator:response": OrchestratorMessageData;
  "orchestrator:thought": OrchestratorMessageData;
  "human:message": HumanMessageData;
  "agent:message": AgentMessageData;
  "agent:error": AgentErrorData;
  "agent:question": AgentQuestionData;
  "automation:run:output": AutomationRunOutputData;
}

export type KnownEventType = keyof EventDataMap;

// ---------------------------------------------------------------------------
// Event — discriminated by `type`
// ---------------------------------------------------------------------------

interface EventBase {
  id: string;
  task: string;
  actor: Actor;
  ts: string;
}

/** A typed event whose `type` is one of the known event types. */
export type TypedEvent<T extends KnownEventType = KnownEventType> = EventBase & {
  type: T;
  data: EventDataMap[T];
};

/** An event with an unrecognized type (e.g. task:state:*, future additions). */
export interface UntypedEvent extends EventBase {
  type: string;
  data: Record<string, unknown>;
}

/** Union of all events. Consumer code can narrow via `isEventType()`. */
export type Event = UntypedEvent;

/**
 * Type guard to narrow an Event to a specific known type with typed data.
 * Returns the event with its `data` field typed according to the EventDataMap.
 */
export function isEventType<T extends KnownEventType>(
  event: Event,
  type: T,
): event is Event & { type: T; data: EventDataMap[T] } {
  return event.type === type;
}

export interface SlotUtilization {
  active: number;
  max: number;
}

export interface Snapshot {
  mode: Mode;
  projects: Project[];
  tasks: Task[];
  merge_queue: MergeQueueEntry[];
  slot_utilization: SlotUtilization;
  human_present: boolean;
  /** Cargo package version of the server binary. */
  server_version: string;
  /** Current data schema version (bumped on incompatible changes). */
  data_version: number;
}

/** Rebuild scope for self-update mechanism */
export type RebuildScope = "frontend" | "server" | "container";

// ---------------------------------------------------------------------------
// Automations
// ---------------------------------------------------------------------------

export type AutomationState = "active" | "paused" | "disabled";

export type TriggerType = "schedule" | "event" | "manual";

export interface TriggerConfig {
  type: TriggerType;
  cron?: string; // For schedule triggers
  event_type?: string; // For event triggers (e.g., "pr_opened", "issue_created")
}

export interface Automation {
  id: string;
  project_id: string;
  name: string;
  prompt: string;
  compiled_workflow?: string;
  compiled_at?: string;
  trigger: TriggerConfig;
  state: AutomationState;
  created_at: string;
  updated_at: string;
}

export interface AutomationRun {
  id: string;
  automation_id: string;
  status: "pending" | "running" | "completed" | "failed" | "cancelled";
  started_at: string;
  completed_at?: string;
  output?: string;
  error?: string;
}

/** Update status for self-update mechanism */
export interface UpdateStatus {
  available: boolean;
  current_commit: string;
  target_commit?: string;
  rebuild_scope?: RebuildScope;
  commit_summary?: string;
  last_checked?: string;
  applying?: boolean;
}

/** Information about an active container session */
export interface ContainerInfo {
  container_id: string;
  task_id: string;
  started_at: string;
  uptime_secs: number;
}

