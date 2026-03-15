<script lang="ts">
	import '../app.css';
	import { onMount, onDestroy } from 'svelte';
	import { page } from '$app/stores';
	import { initApp, destroyApp, getAppState, changeMode } from '$lib/stores/app.svelte';
	import type { Mode } from '$lib/types';

	let { children } = $props();
	const app = getAppState();

	onMount(() => {
		initApp();
	});

	onDestroy(() => {
		destroyApp();
	});

	function isActive(path: string) {
		const current = $page.url.pathname;
		if (path === '/') return current === '/';
		return current.startsWith(path);
	}

	const modeIcons: Record<Mode, string> = {
		stop: '&#9632;',
		pause: '&#9646;&#9646;',
		play: '&#9654;'
	};

	const modeColors: Record<Mode, string> = {
		stop: 'text-error',
		pause: 'text-warning',
		play: 'text-success'
	};

	const navItems = [
		{ path: '/', label: 'Dashboard' },
		{ path: '/tasks', label: 'Tasks' },
		{ path: '/merge-queue', label: 'Merge Queue' },
		{ path: '/events', label: 'Events' }
	];

	function taskCounts() {
		const tasks = app.tasks;
		const running = tasks.filter((t) => t.state === 'running').length;
		const waiting = tasks.filter((t) => t.state === 'waiting').length;
		const question = tasks.filter((t) => t.state === 'question').length;
		return { running, waiting, question, total: tasks.length };
	}
</script>

<div class="flex h-screen">
	<!-- Sidebar -->
	<aside class="bg-base-200 w-60 flex flex-col border-r border-base-300 shrink-0">
		<div class="p-4 border-b border-base-300">
			<h1 class="text-lg font-bold tracking-tight">Tasks</h1>
			<p class="text-xs opacity-60 mt-0.5">Agent orchestration platform</p>
		</div>

		<!-- Mode control -->
		<div class="p-4 border-b border-base-300">
			<div class="text-xs font-medium opacity-60 mb-2 uppercase tracking-wider">Mode</div>
			<div class="join w-full">
				{#each (['stop', 'pause', 'play'] as const) as m}
					<button
						class="join-item btn btn-sm flex-1"
						class:btn-active={app.mode === m}
						class:btn-error={app.mode === m && m === 'stop'}
						class:btn-warning={app.mode === m && m === 'pause'}
						class:btn-success={app.mode === m && m === 'play'}
						onclick={() => changeMode(m)}
					>
						{m[0].toUpperCase() + m.slice(1)}
					</button>
				{/each}
			</div>
		</div>

		<!-- Status -->
		<div class="p-4 border-b border-base-300 space-y-1">
			<div class="flex justify-between text-xs">
				<span class="opacity-60">Sessions</span>
				<span class="font-mono">{app.slotActive}/{app.slotMax}</span>
			</div>
			<div class="flex justify-between text-xs">
				<span class="opacity-60">Running</span>
				<span class="font-mono">{taskCounts().running}</span>
			</div>
			<div class="flex justify-between text-xs">
				<span class="opacity-60">Waiting</span>
				<span class="font-mono">{taskCounts().waiting}</span>
			</div>
			{#if taskCounts().question > 0}
				<div class="flex justify-between text-xs text-warning">
					<span>Questions</span>
					<span class="font-mono">{taskCounts().question}</span>
				</div>
			{/if}
			<div class="flex justify-between text-xs">
				<span class="opacity-60">Merge queue</span>
				<span class="font-mono">{app.mergeQueue.filter((e) => e.status === 'pending' || e.status === 'approved').length}</span>
			</div>
		</div>

		<!-- Nav -->
		<nav class="flex-1 p-2">
			{#each navItems as item}
				<a
					href={item.path}
					class="block px-3 py-2 rounded-lg text-sm transition-colors"
					class:bg-base-300={isActive(item.path)}
					class:font-medium={isActive(item.path)}
					class:opacity-70={!isActive(item.path)}
					class:hover:opacity-100={!isActive(item.path)}
					class:hover:bg-base-300={!isActive(item.path)}
				>
					{item.label}
				</a>
			{/each}
		</nav>

		<!-- Connection status -->
		<div class="p-4 border-t border-base-300">
			{#if app.error}
				<div class="flex items-center gap-2 text-xs text-error">
					<span class="w-2 h-2 rounded-full bg-error"></span>
					Disconnected
				</div>
			{:else}
				<div class="flex items-center gap-2 text-xs text-success">
					<span class="w-2 h-2 rounded-full bg-success"></span>
					Connected
				</div>
			{/if}
		</div>
	</aside>

	<!-- Main content -->
	<main class="flex-1 overflow-auto">
		{@render children()}
	</main>
</div>
