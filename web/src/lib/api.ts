import type { Snapshot, Task, Project, MergeQueueEntry, Mode, Event } from './types';

const BASE = '/api';

async function get<T>(path: string): Promise<T> {
	const res = await fetch(`${BASE}${path}`);
	if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
	return res.json();
}

async function post<T>(path: string, body?: unknown): Promise<T> {
	const res = await fetch(`${BASE}${path}`, {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: body ? JSON.stringify(body) : undefined
	});
	if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
	return res.json();
}

/** GET /api/snapshot - Full system state (spec 16.3). */
export function getSnapshot(): Promise<Snapshot> {
	return get('/snapshot');
}

/** GET /api/tasks */
export function getTasks(): Promise<Task[]> {
	return get('/tasks');
}

/** GET /api/tasks/:id */
export function getTask(id: string): Promise<Task> {
	return get(`/tasks/${id}`);
}

/** GET /api/tasks/:id/events */
export function getTaskEvents(id: string): Promise<Event[]> {
	return get(`/tasks/${id}/events`);
}

/** GET /api/projects */
export function getProjects(): Promise<Project[]> {
	return get('/projects');
}

/** GET /api/merge-queue */
export function getMergeQueue(): Promise<MergeQueueEntry[]> {
	return get('/merge-queue');
}

/** GET /api/mode */
export function getMode(): Promise<{ mode: Mode }> {
	return get('/mode');
}

/** POST /api/mode */
export function setMode(mode: Mode): Promise<{ mode: Mode }> {
	return post('/mode', { mode });
}

/** POST /api/merge-queue/:id/approve */
export async function approveMerge(id: string): Promise<void> {
	await fetch(`${BASE}/merge-queue/${id}/approve`, { method: 'POST' });
}

/** POST /api/merge-queue/:id/reject */
export async function rejectMerge(id: string): Promise<void> {
	await fetch(`${BASE}/merge-queue/${id}/reject`, { method: 'POST' });
}

/** POST /api/merge-queue/flush */
export async function flushMergeQueue(): Promise<string[]> {
	return post('/merge-queue/flush');
}

/** Subscribe to the live SSE event stream. */
export function subscribeEvents(
	onEvent: (event: Event) => void,
	opts?: { pattern?: string; taskId?: string }
): () => void {
	const params = new URLSearchParams();
	if (opts?.pattern) params.set('pattern', opts.pattern);
	if (opts?.taskId) params.set('task_id', opts.taskId);

	const url = `${BASE}/events${params.toString() ? '?' + params : ''}`;
	const source = new EventSource(url);

	source.onmessage = (e) => {
		try {
			onEvent(JSON.parse(e.data));
		} catch {
			// ignore parse errors
		}
	};

	return () => source.close();
}
