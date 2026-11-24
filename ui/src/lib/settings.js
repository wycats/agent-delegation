const TOKEN_KEY = 'delegation-settings-token';

let token = '';

// Takes the token from the URL fragment (first load) or sessionStorage (reload).
export function initToken() {
	const fragment = location.hash.slice(1);
	if (fragment) {
		sessionStorage.setItem(TOKEN_KEY, fragment);
		history.replaceState(history.state, '', location.pathname + location.search);
	}
	token = fragment || sessionStorage.getItem(TOKEN_KEY) || '';
	return token !== '';
}

// GET when `body` is omitted, POST otherwise. Throws an Error carrying `status`.
export async function api(path, body) {
	const headers = { Authorization: `Bearer ${token}` };
	if (body !== undefined) headers['Content-Type'] = 'application/json';
	const response = await fetch(path, {
		method: body === undefined ? 'GET' : 'POST',
		headers,
		body: body === undefined ? undefined : JSON.stringify(body)
	});
	const data = await response.json().catch(() => null);
	if (!response.ok) {
		const error = new Error(data?.error ?? `Request failed (${response.status})`);
		error.status = response.status;
		throw error;
	}
	return data;
}

// Raw TOML values may be strings or opaque structures; render either as text.
export const text = (value) =>
	value == null ? '' : typeof value === 'string' ? value : JSON.stringify(value);

export const show = (value) => value?.label ?? text(value);

export function backendLabel(target) {
	if (target?.backend === 'claude') return 'Claude Code';
	if (target?.backend === 'google') return 'Google Antigravity';
	if (target?.backend === 'ahp') return target.assignment ? 'Saved VS Code assignment' : 'VS Code chat';
	return text(target?.backend) || 'Unknown backend';
}
