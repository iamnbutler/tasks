/** Matches Rust models — spec Section 5. */

export type Mode = 'stop' | 'pause' | 'play';

export type TaskState =
	| 'waiting'
	| 'blocked'
	| 'running'
	| 'question'
	| 'testing'
	| 'awaiting_merge'
	| 'conflict'
	| 'completed'
	| 'failed'
	| 'cancelled';

export type TaskSource =
	| { type: 'github_issue'; owner: string; repo: string; number: number }
	| { type: 'github_pr'; owner: string; repo: string; number: number }
	| { type: 'internal' };

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
	created_at: string;
	updated_at: string;
}

export interface Project {
	id: string;
	repo: string;
	default_branch: string;
	config: Record<string, unknown>;
}

export type MergeStatus = 'pending' | 'approved' | 'rejected' | 'merged' | 'conflict';

export interface MergeQueueEntry {
	id: string;
	task_id: string;
	pr_url: string | null;
	status: MergeStatus;
	queued_at: string;
}

export type Actor = 'human' | 'orchestrator' | 'scheduler' | 'agent' | 'system';

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
