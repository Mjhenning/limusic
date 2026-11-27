// One owner for the integration switches that live in more than one place: the
// Integrations settings tab and the titlebar menu. The backend remains the
// authority; this store only mirrors what it accepted.
import * as api from './api';
import { prefs } from './player.svelte';
import { applyLtState } from './lt.svelte';

export const LT_CLIENT_CONTROL_KEY = 'lt_client_control';

export const integrations = $state({
	controlApi: false,
	tuna: false,
	/// Guest opt-in for player buttons in a shared room. Independent of `controlApi`; available in
	/// both the connected room panel and the player bar, and off until the user enables it.
	clientControl: false,
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
	const [settings, all] = await Promise.all([
		api.getIntegrationSettings(),
		api.getSettings()
	]);
	// Missing means never opted in. Keep this a plain string setting so the Rust transport guard
	// and the badge read the same persisted value.
	integrations.clientControl = all[LT_CLIENT_CONTROL_KEY] === 'true';
	syncIntegrations(settings);
	return settings;
}

/** Toggle only this desktop's player buttons. The Control API server is untouched. */
export async function setClientControl(on: boolean): Promise<void> {
	const before = integrations.clientControl;
	integrations.clientControl = on;
	try {
		await api.ltSetClientControl(on);
		// The backend pushes the capability to the hidden link; refresh the roster immediately so
		// both the room panel and the player-bar badge reflect the new state.
		applyLtState(await api.ltGetState());
	} catch (e) {
		integrations.clientControl = before;
		throw e;
	}
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

/** Set the host opt-in. The hidden link is keyed by the room code, so this can be toggled live:
 *  on opens it immediately, off closes it and guests drop their badges. The relay never changes. */
export async function setSharedControlEnabled(on: boolean): Promise<api.IntegrationSettings> {
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
