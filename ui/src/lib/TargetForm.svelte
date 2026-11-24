<script>
	import { onMount, tick, untrack } from 'svelte';
	import { api, show, text } from './settings.js';

	let { entry = null, busy = false, error = '', onsave, oncancel, ondelete, onreload = null } = $props();

	// The form edits a copy; the original raw values are kept so untouched
	// fields (including opaque IDs and references) are sent back unchanged.
	const original = untrack(() => entry);
	const o = original?.target ?? {};
	const origKind = !original ? null : o.backend !== 'ahp' ? o.backend : o.assignment ? 'saved' : 'ahp';
	const ahp = origKind === 'ahp' ? o : {};

	let name = $state(original?.name ?? '');
	let kind = $state(origKind ?? 'claude');
	let model = $state(origKind === 'ahp' || origKind === 'saved' ? '' : text(o.model));
	let config = $state(text(o.assignment?.config));
	let receipt = $state(text(o.assignment?.receipt));
	let code = $state(text(ahp.code));
	let profile = $state(text(ahp.user_data_dir));
	let session = $state.raw(ahp.session ?? null);
	let chat = $state.raw(ahp.chat ?? null);
	let chatModel = $state.raw(ahp.model ?? null);

	let sessions = $state.raw(null);
	let chats = $state.raw(null);
	let models = $state.raw(null);
	let discovering = $state('');
	let discoverError = $state('');
	let invalid = $state('');
	let confirming = $state(false);
	let nameInput;
	let keepButton = $state();
	let request = 0;

	const withKept = (list, kept) =>
		kept == null || list?.some((item) => item.id === kept)
			? (list ?? [])
			: [{ id: kept, label: `Current: ${text(kept)}` }, ...(list ?? [])];

	let sameEditor = $derived(code === text(ahp.code) && profile === text(ahp.user_data_dir));
	let sameSession = $derived(sameEditor && session === ahp.session);
	let sessionOptions = $derived(withKept(sessions, sameEditor ? ahp.session : null));
	let chatOptions = $derived(withKept(chats, sameSession ? ahp.chat : null));
	let modelOptions = $derived(withKept(models, sameSession ? ahp.model : null));
	let resolved = $derived(entry?.resolved ?? {});
	let details = $derived([
		['Chat', resolved.chat],
		['Editor', resolved.code],
		['Profile', resolved.user_data_dir],
		['Session', resolved.session],
		['Model', resolved.model]
	]);

	onMount(() => nameInput.focus());

	const ref = (value, raw) => (value === text(raw) ? raw : value.trim());

	function editor() {
		const refs = { code: ref(code, ahp.code), user_data_dir: ref(profile, ahp.user_data_dir) };
		return Object.fromEntries(Object.entries(refs).filter(([, value]) => value != null && value !== ''));
	}

	function editorChanged() {
		request++;
		discovering = discoverError = '';
		sessions = chats = models = session = chat = chatModel = null;
	}

	function pickSession(next) {
		session = next;
		chats = models = chat = chatModel = null;
		loadDetails();
	}

	async function discover(body, apply) {
		const id = ++request;
		discoverError = '';
		discovering = body.session == null ? 'Discovering sessions…' : 'Loading chats and models…';
		try {
			const found = await api('/api/discover', body);
			if (id !== request) return false;
			apply(found);
			discovering = '';
			return true;
		} catch (e) {
			if (id === request) {
				discoverError = e.message;
				discovering = '';
			}
			return false;
		}
	}

	async function discoverSessions() {
		const ok = await discover(editor(), (found) => {
			sessions = found.sessions ?? [];
			if (!sessionOptions.some((item) => item.id === session)) session = chat = chatModel = chats = models = null;
		});
		if (ok && session != null) await loadDetails();
	}

	function loadDetails() {
		return discover({ ...editor(), session }, (found) => {
			chats = found.chats ?? [];
			models = found.models ?? [];
			if (!chatOptions.some((item) => item.id === chat)) chat = null;
			if (!modelOptions.some((item) => item.id === chatModel)) chatModel = null;
		});
	}

	function unchanged() {
		if (kind !== origKind) return false;
		if (kind === 'saved') return config === text(o.assignment.config) && receipt === text(o.assignment.receipt);
		if (kind === 'ahp')
			return (
				code === text(o.code) &&
				profile === text(o.user_data_dir) &&
				session === o.session &&
				chat === o.chat &&
				chatModel === o.model
			);
		return model === text(o.model);
	}

	function buildTarget() {
		if (kind === 'saved') {
			if (!config.trim() || !receipt.trim()) return 'Enter both assignment references.';
			const assignment = { config: ref(config, o.assignment?.config), receipt: ref(receipt, o.assignment?.receipt) };
			return { backend: 'ahp', assignment };
		}
		if (kind === 'ahp') {
			if (session == null || chat == null || chatModel == null) return 'Select a session, chat and model.';
			return { backend: 'ahp', ...editor(), session, chat, model: chatModel };
		}
		if (!model.trim()) return 'Enter a model.';
		return { backend: kind, model: ref(model, o.model) };
	}

	function submit(event) {
		event.preventDefault();
		if (busy) return;
		invalid = '';
		const trimmed = name.trim();
		if (!trimmed) return (invalid = 'Enter a target name.');
		// Name-only edits omit the target so the file keeps its comments and reference syntax.
		if (unchanged()) return onsave({ name: trimmed });
		const target = buildTarget();
		if (typeof target === 'string') return (invalid = target);
		onsave({ name: trimmed, target });
	}

	async function askDelete() {
		confirming = true;
		await tick();
		keepButton?.focus();
	}

	const emptyText = (list, noun) => (list ? `No ${noun} found` : discovering ? 'Loading…' : 'Not loaded yet');
</script>

{#snippet picker(id, label, options, get, set, empty)}
	<div class="field">
		<label for={id}>{label}</label>
		<select {id} bind:value={get, set} disabled={!options.length}>
			<option value={null} disabled>{options.length ? `Select a ${label.toLowerCase()}…` : empty}</option>
			{#each options as option}
				<option value={option.id}>{option.label}</option>
			{/each}
		</select>
	</div>
{/snippet}

<form aria-label={original ? `Editing ${original.name}` : 'New target'} onsubmit={submit}>
	<div class="grid">
		<div class="field">
			<label for="target-name">Target name</label>
			<input id="target-name" bind:this={nameInput} bind:value={name} required autocomplete="off" spellcheck="false" />
		</div>
		<div class="field">
			<label for="target-backend">Backend</label>
			<select id="target-backend" bind:value={kind}>
				<option value="claude">Claude Code</option>
				<option value="google">Google Antigravity</option>
				<option value="ahp">VS Code chat</option>
				<option value="saved">Saved VS Code assignment</option>
			</select>
		</div>
	</div>

	{#if entry?.error}
		<p class="error">{entry.error}</p>
	{/if}

	{#if kind === 'saved'}
		{#if entry?.resolved && config === text(o.assignment?.config) && receipt === text(o.assignment?.receipt)}
		<dl>
			{#each details as [label, value]}
				<div><dt>{label}</dt><dd>{show(value) || '—'}</dd></div>
			{/each}
		</dl>
		{/if}
		<div class="grid">
			<div class="field">
				<label for="target-config">Config reference</label>
				<input id="target-config" bind:value={config} autocomplete="off" spellcheck="false" />
			</div>
			<div class="field">
				<label for="target-receipt">Receipt reference</label>
				<input id="target-receipt" bind:value={receipt} autocomplete="off" spellcheck="false" />
			</div>
		</div>
	{:else if kind === 'ahp'}
		<div class="grid">
			<div class="field">
				<label for="target-code">Chat editor <span>optional</span></label>
				<input id="target-code" bind:value={code} oninput={editorChanged} placeholder="code" autocomplete="off" spellcheck="false" />
			</div>
			<div class="field">
				<label for="target-profile">Profile directory <span>optional</span></label>
				<input id="target-profile" bind:value={profile} oninput={editorChanged} autocomplete="off" spellcheck="false" />
			</div>
		</div>
		<div class="discover">
			<button type="button" onclick={discoverSessions}>Discover sessions</button>
			<span role="status">{discovering}</span>
			{#if discoverError}
				<span class="error" role="alert">{discoverError}</span>
			{/if}
		</div>
		{@render picker('target-session', 'Session', sessionOptions, () => session, pickSession, emptyText(sessions, 'sessions'))}
		<div class="grid">
			{@render picker('target-chat', 'Chat', chatOptions, () => chat, (next) => (chat = next), emptyText(chats, 'chats'))}
			{@render picker('target-chat-model', 'Model', modelOptions, () => chatModel, (next) => (chatModel = next), emptyText(models, 'models'))}
		</div>
	{:else}
		<div class="field">
			<label for="target-model">Model</label>
			<input id="target-model" bind:value={model} autocomplete="off" spellcheck="false" />
		</div>
	{/if}

	{#if invalid || error}
		<p class="error" role="alert">
			{invalid || error}
			{#if !invalid && onreload}
				<button type="button" onclick={onreload}>Discard edits and reload</button>
			{/if}
		</p>
	{/if}

	<div class="actions">
		{#if confirming}
			<span>Delete {original.name}? This cannot be undone.</span>
			<button type="button" class="danger" disabled={busy} onclick={ondelete}>Delete {original.name}</button>
			<button type="button" bind:this={keepButton} onclick={() => (confirming = false)}>Keep target</button>
		{:else}
			<button type="submit" class="primary" disabled={busy}>Save target</button>
			<button type="button" onclick={oncancel}>Cancel</button>
			{#if original}
				<button type="button" class="danger delete" onclick={askDelete}>Delete target</button>
			{/if}
		{/if}
	</div>
</form>

<style>
	form {
		display: grid;
		gap: 0.75rem;
		width: 100%;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(14rem, 1fr));
		gap: 0.75rem;
	}
	.field {
		display: grid;
		gap: 0.25rem;
		min-width: 0;
	}
	label {
		font-weight: 500;
	}
	label span {
		color: var(--muted);
		font-weight: 400;
	}
	dl {
		display: grid;
		gap: 0.25rem;
		margin: 0;
		padding: 0.5rem 0.75rem;
		background: var(--subtle);
		border-radius: 6px;
	}
	dl div {
		display: grid;
		grid-template-columns: 5rem 1fr;
		gap: 0.5rem;
	}
	dt {
		color: var(--muted);
	}
	dd {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.discover,
	.actions {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.5rem;
	}
	.discover span:not(.error) {
		color: var(--muted);
	}
	.error {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.5rem;
		margin: 0;
		color: var(--danger);
	}
	.delete {
		margin-left: auto;
	}
</style>
