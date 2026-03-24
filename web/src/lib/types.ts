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

export interface Event {
  id: string;
  type: string;
  task: string;
  actor: Actor;
  ts: string;
  data: Record<string, unknown>;
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
}

/** Rebuild scope for self-update mechanism */
export type RebuildScope = "frontend" | "server" | "container";

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
