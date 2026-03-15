<script lang="ts">
	import { getAppState } from '$lib/stores/app.svelte';
	import TaskStateBadge from '$lib/components/TaskStateBadge.svelte';
	import type { Task, TaskState } from '$lib/types';

	const app = getAppState();

	function groupByState(tasks: Task[]) {
		const groups: Record<string, Task[]> = {};
		for (const t of tasks) {
			(groups[t.state] ??= []).push(t);
		}
		return groups;
	}

	const stateOrder: TaskState[] = [
		'running',
		'question',
		'testing',
		'waiting',
		'blocked',
		'awaiting_merge',
		'conflict',
		'completed',
		'failed',
		'cancelled'
	];

	function activeTaskCount() {
		return app.tasks.filter(
			(t) => !['completed', 'failed', 'cancelled'].includes(t.state)
		).length;
	}

	function recentTasks() {
		return app.tasks
			.filter((t) => ['running', 'question', 'testing', 'waiting'].includes(t.state))
			.sort((a, b) => new Date(b.updated_at).getTime() - new Date(a.updated_at).getTime())
			.slice(0, 10);
	}

	function recentEvents() {
		return app.events.slice(0, 15);
	}

	function sourceLabel(task: Task): string {
		if (task.source.type === 'github_issue') return `#${task.source.number}`;
		if (task.source.type === 'github_pr') return `PR #${task.source.number}`;
		return 'internal';
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
</script>

<div class="p-6 max-w-6xl mx-auto space-y-6">
	<h2 class="text-2xl font-bold">Dashboard</h2>

	<!-- Stats cards -->
	<div class="grid grid-cols-2 md:grid-cols-4 gap-4">
		<div class="stat bg-base-200 rounded-xl p-4">
			<div class="stat-title text-xs">Active Tasks</div>
			<div class="stat-value text-2xl">{activeTaskCount()}</div>
		</div>
		<div class="stat bg-base-200 rounded-xl p-4">
			<div class="stat-title text-xs">Sessions</div>
			<div class="stat-value text-2xl">{app.slotActive}<span class="text-sm opacity-50">/{app.slotMax}</span></div>
		</div>
		<div class="stat bg-base-200 rounded-xl p-4">
			<div class="stat-title text-xs">Projects</div>
			<div class="stat-value text-2xl">{app.projects.length}</div>
		</div>
		<div class="stat bg-base-200 rounded-xl p-4">
			<div class="stat-title text-xs">Merge Queue</div>
			<div class="stat-value text-2xl">
				{app.mergeQueue.filter((e) => e.status === 'pending' || e.status === 'approved').length}
			</div>
		</div>
	</div>

	<div class="grid grid-cols-1 lg:grid-cols-2 gap-6">
		<!-- Active tasks -->
		<div class="bg-base-200 rounded-xl p-4">
			<h3 class="font-semibold mb-3">Active Tasks</h3>
			{#if recentTasks().length === 0}
				<p class="text-sm opacity-50">No active tasks</p>
			{:else}
				<div class="space-y-2">
					{#each recentTasks() as task}
						<a
							href="/tasks/{task.id}"
							class="flex items-center gap-3 p-2 rounded-lg hover:bg-base-300 transition-colors"
						>
							<TaskStateBadge state={task.state} />
							<div class="flex-1 min-w-0">
								<div class="text-sm font-medium truncate">{task.title}</div>
								<div class="text-xs opacity-50">
									{sourceLabel(task)} &middot; {task.project}
								</div>
							</div>
							<div class="text-xs opacity-40 shrink-0">{timeAgo(task.updated_at)}</div>
						</a>
					{/each}
				</div>
			{/if}
		</div>

		<!-- Recent events -->
		<div class="bg-base-200 rounded-xl p-4">
			<h3 class="font-semibold mb-3">Recent Events</h3>
			{#if recentEvents().length === 0}
				<p class="text-sm opacity-50">No events yet</p>
			{:else}
				<div class="space-y-1">
					{#each recentEvents() as event}
						<div class="flex items-center gap-2 py-1 text-xs">
							<span class="font-mono opacity-40 shrink-0">{timeAgo(event.ts)}</span>
							<span class="badge badge-ghost badge-xs">{event.type}</span>
							{#if event.task !== 'system'}
								<a href="/tasks/{event.task}" class="link link-hover truncate">{event.task}</a>
							{:else}
								<span class="opacity-50">system</span>
							{/if}
						</div>
					{/each}
				</div>
			{/if}
		</div>
	</div>

	<!-- Task state breakdown -->
	{#if app.tasks.length > 0}
		<div class="bg-base-200 rounded-xl p-4">
			<h3 class="font-semibold mb-3">Tasks by State</h3>
			<div class="flex flex-wrap gap-3">
				{#each stateOrder as state}
					{@const count = app.tasks.filter((t) => t.state === state).length}
					{#if count > 0}
						<div class="flex items-center gap-2">
							<TaskStateBadge {state} />
							<span class="text-sm">{count}</span>
						</div>
					{/if}
				{/each}
			</div>
		</div>
	{/if}
</div>
