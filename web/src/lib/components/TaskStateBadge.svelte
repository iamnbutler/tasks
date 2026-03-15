<script lang="ts">
	import type { TaskState } from '$lib/types';

	let { state }: { state: TaskState } = $props();

	const config: Record<TaskState, { label: string; class: string }> = {
		running: { label: 'Running', class: 'badge-success' },
		question: { label: 'Question', class: 'badge-warning' },
		testing: { label: 'Testing', class: 'badge-info' },
		waiting: { label: 'Waiting', class: 'badge-ghost' },
		blocked: { label: 'Blocked', class: 'badge-error badge-outline' },
		awaiting_merge: { label: 'Awaiting Merge', class: 'badge-primary' },
		conflict: { label: 'Conflict', class: 'badge-error' },
		completed: { label: 'Completed', class: 'badge-success badge-outline' },
		failed: { label: 'Failed', class: 'badge-error' },
		cancelled: { label: 'Cancelled', class: 'badge-ghost badge-outline' }
	};

	const c = $derived(config[state] ?? { label: state, class: 'badge-ghost' });
</script>

<span class="badge badge-sm {c.class}">{c.label}</span>
