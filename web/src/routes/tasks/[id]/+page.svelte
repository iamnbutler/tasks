<script lang="ts">
	import { page } from '$app/stores';
	import { onMount } from 'svelte';
	import { getTask, getTaskEvents, subscribeEvents } from '$lib/api';
	import TaskStateBadge from '$lib/components/TaskStateBadge.svelte';
	import type { Task, Event } from '$lib/types';

	const taskId = $derived($page.params.id);

	let task = $state<Task | null>(null);
	let events = $state<Event[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	onMount(() => {
		loadTask();

		// Subscribe to live events for this task.
		const unsub = subscribeEvents(
			(event) => {
				events = [...events, event];
				// Refresh task on state changes.
				if (event.type.startsWith('task:state:')) {
					loadTask();
				}
			},
			{ taskId }
		);

		return unsub;
	});

	async function loadTask() {
		try {
			const [t, e] = await Promise.all([getTask(taskId), getTaskEvents(taskId)]);
			task = t;
			events = e;
			loading = false;
		} catch (e) {
			error = e instanceof Error ? e.message : 'Failed to load task';
			loading = false;
		}
	}

	function sourceLabel(task: Task): string {
		if (task.source.type === 'github_issue')
			return `${task.source.owner}/${task.source.repo}#${task.source.number}`;
		if (task.source.type === 'github_pr')
			return `${task.source.owner}/${task.source.repo} PR #${task.source.number}`;
		return 'Internal';
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
</script>

<div class="p-6 max-w-4xl mx-auto space-y-6">
	{#if loading}
		<div class="flex items-center justify-center py-20">
			<span class="loading loading-spinner loading-lg"></span>
		</div>
	{:else if error}
		<div class="alert alert-error">
			<span>{error}</span>
		</div>
	{:else if task}
		<!-- Header -->
		<div>
			<div class="flex items-center gap-2 text-sm opacity-50 mb-1">
				<a href="/tasks" class="link link-hover">Tasks</a>
				<span>/</span>
				<span class="font-mono">{task.id.slice(0, 8)}</span>
			</div>
			<div class="flex items-start gap-3">
				<h2 class="text-2xl font-bold flex-1">{task.title}</h2>
				<TaskStateBadge state={task.state} />
			</div>
		</div>

		<!-- Metadata -->
		<div class="bg-base-200 rounded-xl p-4 grid grid-cols-2 md:grid-cols-3 gap-4 text-sm">
			<div>
				<div class="text-xs opacity-50 mb-0.5">Source</div>
				{#if sourceUrl(task)}
					<a href={sourceUrl(task)} target="_blank" rel="noopener" class="link link-hover">
						{sourceLabel(task)}
					</a>
				{:else}
					{sourceLabel(task)}
				{/if}
			</div>
			<div>
				<div class="text-xs opacity-50 mb-0.5">Project</div>
				{task.project}
			</div>
			<div>
				<div class="text-xs opacity-50 mb-0.5">Priority</div>
				{task.priority !== null ? task.priority : 'None'}
			</div>
			<div>
				<div class="text-xs opacity-50 mb-0.5">Created</div>
				{formatTime(task.created_at)}
			</div>
			<div>
				<div class="text-xs opacity-50 mb-0.5">Updated</div>
				{formatTime(task.updated_at)}
			</div>
			<div>
				<div class="text-xs opacity-50 mb-0.5">Retries</div>
				{task.retry_count}
			</div>
			{#if task.parent_id}
				<div>
					<div class="text-xs opacity-50 mb-0.5">Parent</div>
					<a href="/tasks/{task.parent_id}" class="link link-hover font-mono text-xs">
						{task.parent_id.slice(0, 8)}
					</a>
				</div>
			{/if}
			{#if task.blocked_by.length > 0}
				<div class="col-span-2">
					<div class="text-xs opacity-50 mb-0.5">Blocked by</div>
					<div class="flex gap-2 flex-wrap">
						{#each task.blocked_by as dep}
							<a href="/tasks/{dep}" class="badge badge-sm badge-ghost font-mono">{dep.slice(0, 8)}</a>
						{/each}
					</div>
				</div>
			{/if}
		</div>

		<!-- Labels -->
		{#if task.labels.length > 0}
			<div class="flex gap-2 flex-wrap">
				{#each task.labels as label}
					<span class="badge badge-outline badge-sm">{label}</span>
				{/each}
			</div>
		{/if}

		<!-- Description -->
		{#if task.description}
			<div class="bg-base-200 rounded-xl p-4">
				<h3 class="font-semibold mb-2 text-sm">Description</h3>
				<div class="prose prose-sm max-w-none opacity-80 whitespace-pre-wrap">
					{task.description}
				</div>
			</div>
		{/if}

		<!-- Event timeline -->
		<div class="bg-base-200 rounded-xl p-4">
			<h3 class="font-semibold mb-3 text-sm">Events ({events.length})</h3>
			{#if events.length === 0}
				<p class="text-sm opacity-50">No events recorded</p>
			{:else}
				<div class="space-y-2 max-h-96 overflow-auto">
					{#each events as event}
						<div class="flex items-start gap-3 py-1.5 border-b border-base-300 last:border-0">
							<span class="text-xs font-mono opacity-40 shrink-0 pt-0.5 w-16">
								{timeAgo(event.ts)}
							</span>
							<span class="badge badge-ghost badge-xs shrink-0 mt-0.5">{event.type}</span>
							<span class="badge badge-outline badge-xs shrink-0 mt-0.5">{event.actor}</span>
							{#if Object.keys(event.data).length > 0}
								<span class="text-xs font-mono opacity-40 truncate">
									{JSON.stringify(event.data)}
								</span>
							{/if}
						</div>
					{/each}
				</div>
			{/if}
		</div>
	{/if}
</div>
