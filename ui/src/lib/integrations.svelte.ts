// One owner for the integration switches that live in more than one place: the
// Integrations settings tab and the titlebar menu. The backend remains the
// authority; this store only mirrors what it accepted.
import * as api from './api';
import { prefs } from './player.svelte';
import { applyLtState, inRoom, lt } from './lt.svelte';

export const integrations = $state({
	controlApi: false,
	tuna: false,
	loaded: false
});

/** Discord's flag already lives in `prefs`; expose a reader so callers have one menu model.
 * A module cannot export `$derived` state (vite-plugin-svelte rejects it at compile time), so
 * components call this inside their own `$derived`, which still tracks `prefs.discordRpc`. */
export function discordEnabled(): boolean {
	return prefs.discordRpc;
}

export function syncIntegrations(next: api.IntegrationSettings): void {
	integrations.controlApi = next.control_api.enabled;
	integrations.tuna = next.tuna.enabled;
	integrations.loaded = true;
}

export async function loadIntegrations(): Promise<api.IntegrationSettings> {
	const settings = await api.getIntegrationSettings();
	syncIntegrations(settings);
	return settings;
}

/** Read-modify-write so a titlebar toggle cannot clobber other integration settings. */
export async function setIntegrationEnabled(
	kind: 'control' | 'tuna',
	on: boolean
): Promise<api.IntegrationSettings> {
	const settings = await api.getIntegrationSettings();
	if (kind === 'control') settings.control_api.enabled = on;
	else settings.tuna.enabled = on;
	await api.setIntegrationSettings(settings);
	syncIntegrations(settings);
	return settings;
}

const BEFORE_SERVER_KEY = 'lt_shared_control_server_before';
const ACTIVE_SERVER_KEY = 'lt_shared_control_server_active';

/** Set the host opt-in and swap the Listen Together relay with it.
 *
 * This is deliberately a pre-hosting change: switching relay mid-room would drop everybody.
 * Turning on stores the current relay and switches to the configured fork server; turning off
 * restores the stored relay only if the user has not manually picked another one meanwhile.
 */
export async function setSharedControlEnabled(
	on: boolean,
	server: string
): Promise<api.IntegrationSettings> {
	if (inRoom() || lt.requesting || lt.status === 'connecting') {
		throw new Error('Leave Listen Together before changing shared control.');
	}
	const relay = server.trim();
	if (on && !relay) {
		throw new Error('Set a shared-control server in Integrations first.');
	}

	const current = lt.serverUrl.trim();
	let nextServer = current;
	if (on) {
		localStorage.setItem(ACTIVE_SERVER_KEY, relay);
		if (current !== relay) {
			localStorage.setItem(BEFORE_SERVER_KEY, current);
			nextServer = relay;
		} else if (!localStorage.getItem(BEFORE_SERVER_KEY)) {
			localStorage.setItem(BEFORE_SERVER_KEY, current);
		}
	} else {
		const active = localStorage.getItem(ACTIVE_SERVER_KEY) ?? relay;
		if (current === active) {
			nextServer = localStorage.getItem(BEFORE_SERVER_KEY) ?? '';
		}
		localStorage.removeItem(BEFORE_SERVER_KEY);
		localStorage.removeItem(ACTIVE_SERVER_KEY);
	}
	if (nextServer !== current) await api.ltSetServerUrl(nextServer);

	const settings = await api.getIntegrationSettings();
	settings.control_api.shared_control = on;
	await api.setIntegrationSettings(settings);
	syncIntegrations(settings);
	applyLtState(await api.ltGetState());
	return settings;
}

export async function setDiscordEnabled(on: boolean): Promise<void> {
	const before = prefs.discordRpc;
	prefs.discordRpc = on;
	try {
		await api.setSetting('discord_rpc', on ? 'true' : 'false');
	} catch (e) {
		prefs.discordRpc = before;
		throw e;
	}
}
