import { getSnapshot, subscribeEvents, setMode as apiSetMode } from '$lib/api';
import type { Snapshot, Task, Mode, Event, MergeQueueEntry, Project } from '$lib/types';

/** Reactive application state — polls snapshot and subscribes to SSE. */

let snapshot = $state<Snapshot | null>(null);
let events = $state<Event[]>([]);
let connected = $state(false);
let error = $state<string | null>(null);

const MAX_EVENTS = 200;

let unsubscribe: (() => void) | null = null;
let pollTimer: ReturnType<typeof setInterval> | null = null;

export function getAppState() {
	return {
		get snapshot() {
			return snapshot;
		},
		get events() {
			return events;
		},
		get connected() {
			return connected;
		},
		get error() {
			return error;
		},

		get mode(): Mode {
			return snapshot?.mode ?? 'pause';
		},
		get tasks(): Task[] {
			return snapshot?.tasks ?? [];
		},
		get projects(): Project[] {
			return snapshot?.projects ?? [];
		},
		get mergeQueue(): MergeQueueEntry[] {
			return snapshot?.merge_queue ?? [];
		},
		get slotActive(): number {
			return snapshot?.slot_utilization.active ?? 0;
		},
		get slotMax(): number {
			return snapshot?.slot_utilization.max ?? 0;
		},
		get humanPresent(): boolean {
			return snapshot?.human_present ?? false;
		}
	};
}

export async function initApp() {
	await refresh();

	// Poll every 5 seconds for state.
	pollTimer = setInterval(refresh, 5000);

	// Subscribe to SSE for live events.
	unsubscribe = subscribeEvents((event) => {
		events = [event, ...events].slice(0, MAX_EVENTS);
		// Trigger a snapshot refresh on state-changing events.
		if (event.type.startsWith('task:') || event.type.startsWith('merge:') || event.type.startsWith('system:mode')) {
			refresh();
		}
	});

	connected = true;
}

export function destroyApp() {
	if (unsubscribe) unsubscribe();
	if (pollTimer) clearInterval(pollTimer);
	connected = false;
}

async function refresh() {
	try {
		snapshot = await getSnapshot();
		error = null;
	} catch (e) {
		error = e instanceof Error ? e.message : 'Failed to connect';
	}
}

export async function changeMode(mode: Mode) {
	try {
		const result = await apiSetMode(mode);
		if (snapshot) {
			snapshot = { ...snapshot, mode: result.mode };
		}
	} catch (e) {
		error = e instanceof Error ? e.message : 'Failed to change mode';
	}
}
