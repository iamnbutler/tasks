<script lang="ts">
	import { getAppState } from '$lib/stores/app.svelte';
	import type { Event } from '$lib/types';

	const app = getAppState();

	let filterType = $state('all');
	let paused = $state(false);

	const typeGroups = [
		{ label: 'All', value: 'all' },
		{ label: 'Task', value: 'task:' },
		{ label: 'Agent', value: 'agent:' },
		{ label: 'Merge', value: 'merge:' },
		{ label: 'System', value: 'system:' },
		{ label: 'Orchestrator', value: 'orchestrator:' }
	];

	function filteredEvents(): Event[] {
		if (filterType === 'all') return app.events;
		return app.events.filter((e) => e.type.startsWith(filterType));
	}

	function formatTime(ts: string): string {
		return new Date(ts).toLocaleTimeString();
	}

	function timeAgo(ts: string): string {
		const diff = Date.now() - new Date(ts).getTime();
		const secs = Math.floor(diff / 1000);
		if (secs < 5) return 'now';
		if (secs < 60) return `${secs}s ago`;
		const mins = Math.floor(secs / 60);
		if (mins < 60) return `${mins}m ago`;
		return `${Math.floor(mins / 60)}h ago`;
	}

	const eventTypeColors: Record<string, string> = {
		'task:created': 'badge-info',
		'task:state:running': 'badge-success',
		'task:state:question': 'badge-warning',
		'task:state:completed': 'badge-success badge-outline',
		'task:state:failed': 'badge-error',
		'task:state:cancelled': 'badge-ghost',
		'agent:message': 'badge-info badge-outline',
		'agent:question': 'badge-warning',
		'agent:error': 'badge-error',
		'merge:queued': 'badge-primary badge-outline',
		'merge:approved': 'badge-success',
		'merge:completed': 'badge-primary',
		'merge:conflict': 'badge-error',
		'system:started': 'badge-info',
		'system:mode:play': 'badge-success',
		'system:mode:pause': 'badge-warning',
		'system:mode:stop': 'badge-error'
	};
</script>

<div class="p-6 max-w-5xl mx-auto space-y-4">
	<div class="flex items-center justify-between">
		<h2 class="text-2xl font-bold">Events</h2>
		<div class="flex items-center gap-2">
			<span class="text-sm opacity-50">{filteredEvents().length} events</span>
			<button
				class="btn btn-sm btn-ghost"
				class:btn-active={paused}
				onclick={() => (paused = !paused)}
			>
				{paused ? 'Resume' : 'Pause'}
			</button>
		</div>
	</div>

	<!-- Type filter -->
	<div class="flex flex-wrap gap-2">
		{#each typeGroups as group}
			<button
				class="btn btn-xs"
				class:btn-active={filterType === group.value}
				onclick={() => (filterType = group.value)}
			>
				{group.label}
			</button>
		{/each}
	</div>

	<!-- Event stream -->
	<div class="bg-base-200 rounded-xl overflow-hidden">
		<div class="max-h-[calc(100vh-200px)] overflow-auto">
			<table class="table table-xs w-full">
				<thead class="sticky top-0 bg-base-200">
					<tr>
						<th class="w-20">Time</th>
						<th class="w-44">Type</th>
						<th class="w-20">Actor</th>
						<th class="w-24">Task</th>
						<th>Data</th>
					</tr>
				</thead>
				<tbody class="font-mono text-xs">
					{#each filteredEvents() as event}
						<tr class="hover:bg-base-300">
							<td class="opacity-40">{formatTime(event.ts)}</td>
							<td>
								<span class="badge badge-xs {eventTypeColors[event.type] ?? 'badge-ghost'}">
									{event.type}
								</span>
							</td>
							<td class="opacity-60">{event.actor}</td>
							<td>
								{#if event.task !== 'system'}
									<a href="/tasks/{event.task}" class="link link-hover">
										{event.task.slice(0, 8)}
									</a>
								{:else}
									<span class="opacity-40">system</span>
								{/if}
							</td>
							<td class="opacity-40 truncate max-w-xs">
								{Object.keys(event.data).length > 0 ? JSON.stringify(event.data) : ''}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>

			{#if filteredEvents().length === 0}
				<div class="p-8 text-center text-sm opacity-50">
					No events to display. Events will appear here as they occur.
				</div>
			{/if}
		</div>
	</div>
</div>
