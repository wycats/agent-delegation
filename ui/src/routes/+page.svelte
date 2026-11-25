<script>
	import { onMount, tick } from 'svelte';
	import TargetForm from '$lib/TargetForm.svelte';
	import { api, backendLabel, initToken, show } from '$lib/settings.js';

	let snapshot = $state(null);
	let pageError = $state('');
	let form = $state(null); // { original: name | null } while a form is open
	let busy = $state(false);
	let formError = $state('');
	let conflict = $state(false);
	let status = $state('');
	let checks = $state({});
	let closed = $state(false);
	let addButton = $state();

	onMount(() => {
		if (initToken()) load();
		else pageError = 'Missing access token. Open the settings link again.';
	});

	function apply(next) {
		snapshot = next;
		checks = {};
	}

	async function load() {
		try {
			apply(await api('/api/targets'));
			pageError = '';
			return true;
		} catch (e) {
			pageError = e.message;
			return false;
		}
	}

	function openForm(original) {
		form = { original };
		formError = status = '';
		conflict = false;
	}

	async function closeForm(focusName) {
		form = null;
		formError = '';
		conflict = false;
		await tick();
		const edit = [...document.querySelectorAll('[data-edit]')].find((button) => button.dataset.edit === focusName);
		(edit ?? addButton)?.focus();
	}

	async function post(change, message, focusName) {
		if (busy) return;
		busy = true;
		formError = '';
		conflict = false;
		try {
			apply(await api('/api/targets', { revision: snapshot.revision, ...change }));
			status = message;
			closeForm(focusName);
		} catch (e) {
			formError = e.message;
			conflict = e.status === 409;
		} finally {
			busy = false;
		}
	}

	function save({ name, target }) {
		const change = { name };
		if (form.original != null) change.original = form.original;
		if (target) change.target = target;
		post(change, `Saved ${name}.`, name);
	}

	const remove = () => post({ original: form.original, delete: true }, `Deleted ${form.original}.`, null);

	async function reload() {
		if (!(await load())) return;
		await closeForm(form?.original);
		status = 'Loaded the current settings from disk.';
	}

	async function check(name) {
		const revision = snapshot.revision;
		checks[name] = { busy: true };
		let result;
		try {
			result = { message: (await api('/api/check', { name })).message };
		} catch (e) {
			result = { error: e.message };
		}
		if (snapshot.revision === revision) checks[name] = result;
	}

	async function close() {
		try {
			await api('/api/close', {});
			closed = true;
			window.close();
		} catch (e) {
			pageError = e.message;
		}
	}
</script>

<svelte:head>
	<title>Delegation settings</title>
</svelte:head>

<main>
	<header>
		<div>
			<h1>Delegation settings</h1>
			{#if snapshot}
				<p class="path"><code>{snapshot.path}</code></p>
			{/if}
		</div>
		{#if !closed}
			<button onclick={close}>Done</button>
		{/if}
	</header>

	{#if closed}
		<p class="panel">Settings closed. You can close this tab.</p>
	{:else}
		{#if pageError}
			<p class="panel error" role="alert">
				{pageError}
				{#if !snapshot}
					<button onclick={load}>Retry</button>
				{/if}
			</p>
		{/if}

		{#if snapshot}
			<section class="panel" aria-labelledby="targets-heading">
				<h2 id="targets-heading">Targets</h2>
				<p class="usage">Use an existing name with <code>--target</code>. Add a target when you want to name another destination.</p>
				{#if snapshot.targets.length}
					<ul>
						{#each snapshot.targets as t (t.name)}
							{@const result = checks[t.name]}
							<li>
								{#if form?.original === t.name}
									<TargetForm
										entry={t}
										{busy}
										error={formError}
										onsave={save}
										oncancel={() => closeForm(t.name)}
										ondelete={remove}
										onreload={conflict ? reload : null}
									/>
								{:else}
									<div class="info">
										<strong>{t.name}</strong>
										<span>{[backendLabel(t.target), show(t.model ?? t.resolved?.model ?? t.target?.model)].filter(Boolean).join(' · ')}</span>
										{#if t.error}
											<span class="error">{t.error}</span>
										{/if}
										{#if result?.message || result?.error}
											<span class={result.error ? 'error' : 'ok'} role="status">{result.error ?? result.message}</span>
										{/if}
									</div>
									{#if t.target?.backend === 'ahp'}
										<button aria-label="Check {t.name}" disabled={result?.busy} onclick={() => check(t.name)}>
											{result?.busy ? 'Checking…' : 'Check'}
										</button>
									{/if}
									<button aria-label="Edit {t.name}" data-edit={t.name} onclick={() => openForm(t.name)}>Edit</button>
								{/if}
							</li>
						{/each}
					</ul>
				{:else if !form}
					<p class="empty">No targets yet.</p>
				{/if}

				{#if form && form.original === null}
					<div class="new">
						<TargetForm
							{busy}
							error={formError}
							onsave={save}
							oncancel={() => closeForm(null)}
							onreload={conflict ? reload : null}
						/>
					</div>
				{:else}
					<div class="new">
						<button bind:this={addButton} onclick={() => openForm(null)}>Add target</button>
					</div>
				{/if}
			</section>
		{:else if !pageError}
			<p class="panel empty">Loading…</p>
		{/if}

		<p class="status" role="status">{status}</p>
	{/if}
</main>

<style>
	:global(:root) {
		--text: #1c2024;
		--muted: #60646c;
		--border: #d9dde3;
		--subtle: #f6f7f9;
		--accent: #2f5fd0;
		--danger: #b42318;
		--ok: #18794e;
	}
	:global(body) {
		margin: 0;
		background: var(--subtle);
		color: var(--text);
		font: 14px/1.45 Inter, system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif;
	}
	:global(:is(button, input, select)) {
		box-sizing: border-box;
		min-height: 2rem;
		padding: 0.25rem 0.65rem;
		border: 1px solid var(--border);
		border-radius: 6px;
		background: #fff;
		color: inherit;
		font: inherit;
	}
	:global(:is(input, select)) {
		width: 100%;
		min-width: 0;
	}
	:global(button) {
		font-weight: 500;
		white-space: nowrap;
		cursor: pointer;
	}
	:global(button:hover:not(:disabled)) {
		background: var(--subtle);
	}
	:global(button.primary) {
		border-color: var(--accent);
		background: var(--accent);
		color: #fff;
	}
	:global(button.primary:hover:not(:disabled)) {
		background: #274fb0;
	}
	:global(button.danger) {
		color: var(--danger);
	}
	:global(:disabled) {
		opacity: 0.6;
		cursor: default;
	}
	:global(:focus-visible) {
		outline: 2px solid var(--accent);
		outline-offset: 1px;
	}

	main {
		max-width: 44rem;
		margin: 0 auto;
		padding: 1.5rem 1rem 3rem;
	}
	header {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 1rem;
		margin-bottom: 1rem;
	}
	h1 {
		margin: 0;
		font-size: 1.25rem;
		font-weight: 600;
	}
	h2 {
		margin: 0;
		padding: 0.6rem 1rem;
		border-bottom: 1px solid var(--border);
		color: var(--muted);
		font-size: 0.75rem;
		font-weight: 600;
		letter-spacing: 0.04em;
		text-transform: uppercase;
	}
	.path {
		margin: 0.15rem 0 0;
		color: var(--muted);
		overflow-wrap: anywhere;
	}
	code {
		font: 0.85rem ui-monospace, 'Cascadia Code', Consolas, Menlo, monospace;
	}
	.panel {
		margin: 0 0 1rem;
		border: 1px solid var(--border);
		border-radius: 8px;
		background: #fff;
	}
	p.panel {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.75rem;
		padding: 0.75rem 1rem;
	}
	ul {
		margin: 0;
		padding: 0;
		list-style: none;
	}
	li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.5rem;
		padding: 0.6rem 1rem;
		border-bottom: 1px solid var(--border);
	}
	.info {
		display: grid;
		flex: 1 1 12rem;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.info span,
	.empty,
	.status {
		color: var(--muted);
	}
	.info .error,
	.error {
		color: var(--danger);
	}
	.info .ok {
		color: var(--ok);
	}
	.empty {
		margin: 0;
		padding: 0.75rem 1rem;
	}
	.usage { margin: 0; padding: 0.65rem 1rem; color: var(--muted); }
	.new {
		padding: 0.6rem 1rem;
	}
	.status {
		min-height: 1.45em;
		margin: 0;
	}
</style>
