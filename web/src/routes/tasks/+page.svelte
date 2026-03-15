<script lang="ts">
	import { getAppState } from '$lib/stores/app.svelte';
	import TaskStateBadge from '$lib/components/TaskStateBadge.svelte';
	import type { Task, TaskState } from '$lib/types';

	const app = getAppState();

	let filterState = $state<TaskState | 'all'>('all');
	let filterProject = $state<string>('all');
	let search = $state('');

	function filteredTasks(): Task[] {
		let tasks = app.tasks;
		if (filterState !== 'all') {
			tasks = tasks.filter((t) => t.state === filterState);
		}
		if (filterProject !== 'all') {
			tasks = tasks.filter((t) => t.project === filterProject);
		}
		if (search) {
			const q = search.toLowerCase();
			tasks = tasks.filter(
				(t) =>
					t.title.toLowerCase().includes(q) ||
					t.id.toLowerCase().includes(q)
			);
		}
		return tasks.sort(
			(a, b) => new Date(b.updated_at).getTime() - new Date(a.updated_at).getTime()
		);
	}

	function sourceLabel(task: Task): string {
		if (task.source.type === 'github_issue') return `#${task.source.number}`;
		if (task.source.type === 'github_pr') return `PR #${task.source.number}`;
		return 'internal';
	}

	function sourceUrl(task: Task): string | null {
		if (task.source.type === 'github_issue') {
			return `https://github.com/${task.source.owner}/${task.source.repo}/issues/${task.source.number}`;
		}
		if (task.source.type === 'github_pr') {
			return `https://github.com/${task.source.owner}/${task.source.repo}/pull/${task.source.number}`;
		}
		return null;
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

	const states: (TaskState | 'all')[] = [
		'all',
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
</script>

<div class="p-6 max-w-6xl mx-auto space-y-4">
	<div class="flex items-center justify-between">
		<h2 class="text-2xl font-bold">Tasks</h2>
		<span class="text-sm opacity-50">{filteredTasks().length} tasks</span>
	</div>

	<!-- Filters -->
	<div class="flex flex-wrap gap-3">
		<input
			type="text"
			placeholder="Search tasks..."
			class="input input-sm input-bordered w-64"
			bind:value={search}
		/>

		<select class="select select-sm select-bordered" bind:value={filterState}>
			{#each states as s}
				<option value={s}>{s === 'all' ? 'All states' : s.replace('_', ' ')}</option>
			{/each}
		</select>

		{#if app.projects.length > 1}
			<select class="select select-sm select-bordered" bind:value={filterProject}>
				<option value="all">All projects</option>
				{#each app.projects as p}
					<option value={p.id}>{p.repo}</option>
				{/each}
			</select>
		{/if}
	</div>

	<!-- Task list -->
	<div class="bg-base-200 rounded-xl overflow-hidden">
		<table class="table table-sm w-full">
			<thead>
				<tr class="text-xs">
					<th>State</th>
					<th>Task</th>
					<th>Source</th>
					<th>Project</th>
					<th>Priority</th>
					<th>Updated</th>
				</tr>
			</thead>
			<tbody>
				{#each filteredTasks() as task}
					<tr class="hover:bg-base-300 transition-colors">
						<td>
							<TaskStateBadge state={task.state} />
						</td>
						<td>
							<a href="/tasks/{task.id}" class="link link-hover font-medium">
								{task.title}
							</a>
							{#if task.labels.length > 0}
								<div class="flex gap-1 mt-0.5">
									{#each task.labels.slice(0, 3) as label}
										<span class="badge badge-ghost badge-xs">{label}</span>
									{/each}
								</div>
							{/if}
						</td>
						<td class="text-xs">
							{#if sourceUrl(task)}
								<a href={sourceUrl(task)} target="_blank" rel="noopener" class="link link-hover">
									{sourceLabel(task)}
								</a>
							{:else}
								{sourceLabel(task)}
							{/if}
						</td>
						<td class="text-xs opacity-60">{task.project}</td>
						<td class="text-xs font-mono">
							{task.priority !== null ? task.priority : '-'}
						</td>
						<td class="text-xs opacity-50">{timeAgo(task.updated_at)}</td>
					</tr>
				{/each}
			</tbody>
		</table>

		{#if filteredTasks().length === 0}
			<div class="p-8 text-center text-sm opacity-50">
				No tasks match your filters
			</div>
		{/if}
	</div>
</div>
