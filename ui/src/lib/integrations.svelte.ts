// One owner for the integration switches that live in more than one place: the
// Integrations settings tab and the titlebar menu. The backend remains the
// authority; this store only mirrors what it accepted.
import * as api from './api';
import { prefs } from './player.svelte';

export const integrations = $state({
	controlApi: false,
	tuna: false,
	loaded: false
});

/** Discord's flag already lives in `prefs`; expose it here so callers have one menu model. */
export const discordEnabled = $derived(prefs.discordRpc);

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
