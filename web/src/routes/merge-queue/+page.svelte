<script lang="ts">
	import { getAppState, changeMode } from '$lib/stores/app.svelte';
	import { approveMerge, rejectMerge, flushMergeQueue } from '$lib/api';
	import MergeStatusBadge from '$lib/components/MergeStatusBadge.svelte';
	import type { MergeQueueEntry, MergeStatus } from '$lib/types';

	const app = getAppState();

	let filterStatus = $state<MergeStatus | 'all'>('all');

	function filteredEntries(): MergeQueueEntry[] {
		let entries = app.mergeQueue;
		if (filterStatus !== 'all') {
			entries = entries.filter((e) => e.status === filterStatus);
		}
		return entries;
	}

	function formatTime(ts: string): string {
		return new Date(ts).toLocaleString();
	}

	function timeAgo(ts: string): string {
		const diff = Date.now() - new Date(ts).getTime();
		const mins = Math.floor(diff / 60000);
		if (mins < 1) return 'just now';
		if (mins < 60) return `${mins}m ago`;
		const hours = Math.floor(mins / 60);
		if (hours < 24) return `${hours}h ago`;
		return `${Math.floor(hours / 24)}d ago`;
	}

	async function handleApprove(id: string) {
		await approveMerge(id);
	}

	async function handleReject(id: string) {
		await rejectMerge(id);
	}

	async function handleFlush() {
		await flushMergeQueue();
	}

	function pendingCount() {
		return app.mergeQueue.filter((e) => e.status === 'pending').length;
	}

	function approvedCount() {
		return app.mergeQueue.filter((e) => e.status === 'approved').length;
	}
</script>

<div class="p-6 max-w-4xl mx-auto space-y-4">
	<div class="flex items-center justify-between">
		<h2 class="text-2xl font-bold">Merge Queue</h2>
		<div class="flex gap-2">
			{#if app.mode === 'pause' && approvedCount() > 0}
				<button class="btn btn-sm btn-primary" onclick={handleFlush}>
					Flush ({approvedCount()})
				</button>
			{/if}
		</div>
	</div>

	<!-- Mode info -->
	<div class="bg-base-200 rounded-xl p-3 text-sm flex items-center gap-2">
		{#if app.mode === 'stop'}
			<span class="badge badge-error badge-sm">Stop</span>
			<span class="opacity-60">Merge queue is held. No merges will happen.</span>
		{:else if app.mode === 'pause'}
			<span class="badge badge-warning badge-sm">Pause</span>
			<span class="opacity-60">Merge queue is held. Review and approve items, then Flush.</span>
		{:else}
			<span class="badge badge-success badge-sm">Play</span>
			<span class="opacity-60">Orchestrator has merge authority. Merges happen automatically.</span>
		{/if}
	</div>

	<!-- Filters -->
	<div class="flex gap-3">
		<select class="select select-sm select-bordered" bind:value={filterStatus}>
			<option value="all">All statuses</option>
			<option value="pending">Pending</option>
			<option value="approved">Approved</option>
			<option value="rejected">Rejected</option>
			<option value="merged">Merged</option>
			<option value="conflict">Conflict</option>
		</select>
	</div>

	<!-- Queue -->
	<div class="bg-base-200 rounded-xl overflow-hidden">
		<table class="table table-sm w-full">
			<thead>
				<tr class="text-xs">
					<th>Status</th>
					<th>Task</th>
					<th>PR</th>
					<th>Queued</th>
					<th>Actions</th>
				</tr>
			</thead>
			<tbody>
				{#each filteredEntries() as entry}
					<tr class="hover:bg-base-300 transition-colors">
						<td>
							<MergeStatusBadge status={entry.status} />
						</td>
						<td>
							<a href="/tasks/{entry.task_id}" class="link link-hover font-mono text-xs">
								{entry.task_id.slice(0, 12)}
							</a>
						</td>
						<td class="text-xs">
							{#if entry.pr_url}
								<a href={entry.pr_url} target="_blank" rel="noopener" class="link link-hover">
									View PR
								</a>
							{:else}
								<span class="opacity-40">-</span>
							{/if}
						</td>
						<td class="text-xs opacity-50">{timeAgo(entry.queued_at)}</td>
						<td>
							{#if entry.status === 'pending'}
								<div class="flex gap-1">
									<button
										class="btn btn-xs btn-success"
										onclick={() => handleApprove(entry.id)}
									>
										Approve
									</button>
									<button
										class="btn btn-xs btn-error btn-outline"
										onclick={() => handleReject(entry.id)}
									>
										Reject
									</button>
								</div>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>

		{#if filteredEntries().length === 0}
			<div class="p-8 text-center text-sm opacity-50">
				Merge queue is empty
			</div>
		{/if}
	</div>
</div>
