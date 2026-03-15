<script lang="ts">
	import type { MergeStatus } from '$lib/types';

	let { status }: { status: MergeStatus } = $props();

	const config: Record<MergeStatus, { label: string; class: string }> = {
		pending: { label: 'Pending', class: 'badge-warning' },
		approved: { label: 'Approved', class: 'badge-success' },
		rejected: { label: 'Rejected', class: 'badge-error' },
		merged: { label: 'Merged', class: 'badge-primary' },
		conflict: { label: 'Conflict', class: 'badge-error badge-outline' }
	};

	const c = $derived(config[status] ?? { label: status, class: 'badge-ghost' });
</script>

<span class="badge badge-sm {c.class}">{c.label}</span>
