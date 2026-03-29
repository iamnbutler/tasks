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
// Event payload types — shared type definitions for backend event data
// ---------------------------------------------------------------------------

/** agent:message — stdout/stderr from agent session */
export interface AgentMessageData {
  text: string;
  stream?: string;
}

/** agent:question — agent asking for user input */
export interface AgentQuestionData {
  question?: string;
  message?: string;
  text?: string;
}

/** agent:error — agent error condition */
export interface AgentErrorData {
  text?: string;
  [key: string]: unknown;
}

/** human:message — user message to agent or orchestrator */
export interface HumanMessageData {
  message: string;
  source?: string;
}

/** orchestrator:decision — merge decision */
export interface OrchestratorDecisionData {
  task_id?: string;
  approved: boolean;
  reasoning?: string;
  entry_id?: string;
}

/** orchestrator:feedback — feedback sent to agent */
export interface OrchestratorFeedbackData {
  task_id?: string;
  feedback?: string;
  context?: string;
}

/** orchestrator:escalation — escalation for human review */
export interface OrchestratorEscalationData {
  action?: string;
  reasoning?: string;
  reason?: string;
  message?: string;
  entry_id?: string;
  pr_url?: string;
  from?: string;
  to?: string;
}

/** orchestrator:message / orchestrator:response / orchestrator:thought */
export interface OrchestratorMessageData {
  message?: string;
  error?: boolean;
}

/** automation:run:output — streaming output chunk */
export interface AutomationRunOutputData {
  chunk?: string;
}

/** task:state:* and task:created — lifecycle events (data is often empty or minimal) */
export interface TaskLifecycleData {
  reason?: string;
  message?: string;
  [key: string]: unknown;
}

// ---------------------------------------------------------------------------
// Claude Code protocol message types (parsed from agent:message text field)
// ---------------------------------------------------------------------------

export interface ClaudeTextBlock {
  type: "text";
  text: string;
}

export interface ClaudeThinkingBlock {
  type: "thinking";
  thinking?: string;
}

export interface ClaudeToolInput {
  file_path?: string;
  filePath?: string;
  path?: string;
  pattern?: string;
  command?: string;
  description?: string;
  [key: string]: unknown;
}

export interface ClaudeToolUseBlock {
  type: "tool_use";
  name: string;
  input?: ClaudeToolInput;
}

export interface ClaudeToolResultBlock {
  type: "tool_result";
  content?: string;
}

export type ClaudeContentBlock =
  | ClaudeTextBlock
  | ClaudeThinkingBlock
  | ClaudeToolUseBlock
  | ClaudeToolResultBlock;

export interface ClaudeResultMessage {
  type: "result";
  result?: { text?: string };
}

export interface ClaudeSystemMessage {
  type: "system";
}

export interface ClaudeAssistantMessage {
  type: "assistant" | undefined;
  message?: { content?: ClaudeContentBlock[] };
  content?: ClaudeContentBlock[];
}

export type ClaudeProtocolMessage =
  | ClaudeResultMessage
  | ClaudeSystemMessage
  | ClaudeAssistantMessage;

// ---------------------------------------------------------------------------
// Event — discriminated union for compile-time type safety
// ---------------------------------------------------------------------------

interface EventBase {
  id: string;
  task: string;
  actor: Actor;
  ts: string;
}

export type Event =
  | (EventBase & { type: "agent:message"; data: AgentMessageData })
  | (EventBase & { type: "agent:question"; data: AgentQuestionData })
  | (EventBase & { type: "agent:error"; data: AgentErrorData })
  | (EventBase & { type: "human:message"; data: HumanMessageData })
  | (EventBase & { type: "orchestrator:decision"; data: OrchestratorDecisionData })
  | (EventBase & { type: "orchestrator:feedback"; data: OrchestratorFeedbackData })
  | (EventBase & { type: "orchestrator:escalation"; data: OrchestratorEscalationData })
  | (EventBase & { type: "orchestrator:message"; data: OrchestratorMessageData })
  | (EventBase & { type: "orchestrator:response"; data: OrchestratorMessageData })
  | (EventBase & { type: "orchestrator:thought"; data: OrchestratorMessageData })
  | (EventBase & { type: "automation:run:output"; data: AutomationRunOutputData })
  | (EventBase & { type: "automation:run:started"; data: TaskLifecycleData })
  | (EventBase & { type: "automation:run:completed"; data: TaskLifecycleData })
  | (EventBase & { type: "automation:run:failed"; data: TaskLifecycleData })
  | (EventBase & { type: "automation:run:cancelled"; data: TaskLifecycleData })
  | (EventBase & { type: "task:created"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:running"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:question"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:waiting"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:blocked"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:testing"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:awaiting_merge"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:conflict"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:changes_requested"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:completed"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:failed"; data: TaskLifecycleData })
  | (EventBase & { type: "task:state:cancelled"; data: TaskLifecycleData });

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

