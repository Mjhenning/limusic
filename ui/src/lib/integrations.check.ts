// Self-check for the Integrations settings added on control-api. There is no UI test runner;
// Node 22 runs TypeScript directly (see queue.check.ts):
//
//     node --experimental-strip-types ui/src/lib/integrations.check.ts
//
// It guards the token-regenerate confirmation against regressing to a bare window.confirm: the
// webview used by this app does not allow those dialogs, and its shim can return a truthy promise
// that silently skips the "cancel" branch. It also scans the Rust integration code for the same
// three global dialogs, which have no place in a Tauri backend.
//
// @ts-nocheck: it reads the sources with node:fs, and ui/ has no Node types for svelte-check.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, extname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SETTINGS } from './settings-search.ts';

function ok(cond: boolean, what: string): void {
	if (!cond) throw new Error(`FAIL: ${what}`);
}

function rustFiles(dir: string): string[] {
	return readdirSync(dir).flatMap((name) => {
		const path = join(dir, name);
		return statSync(path).isDirectory()
			? rustFiles(path)
			: extname(path) === '.rs'
				? [path]
				: [];
	});
}

const here = dirname(fileURLToPath(import.meta.url));
const component = join(here, 'components', 'IntegrationsSettings.svelte');
const discord = join(here, 'components', 'DiscordSettings.svelte');
const titlebar = join(here, 'components', 'Titlebar.svelte');
const dialog = join(here, 'components', 'SettingsDialog.svelte');
const store = join(here, 'integrations.svelte.ts');
const root = join(here, '..', '..', '..');
const files = [
	component,
	discord,
	titlebar,
	dialog,
	store,
	join(here, 'api.ts'),
	join(root, 'src-tauri', 'src', 'commands.rs'),
	...rustFiles(join(root, 'src-tauri', 'src', 'control_api')),
	...rustFiles(join(root, 'src-tauri', 'src', 'integrations'))
];

const dialogCall = /\b(?:confirm|alert|prompt)\s*\(/;
for (const file of files) {
	const source = readFileSync(file, 'utf8');
	const line = source.split('\n').findIndex((text) => dialogCall.test(text)) + 1;
	ok(line === 0, `${file}:${line} uses a bare confirm/alert/prompt dialog`);
}

const ui = readFileSync(component, 'utf8');
ok(ui.includes("import * as AlertDialog from '$lib/components/ui/alert-dialog';"), 'missing AlertDialog import');
ok(ui.includes('bind:open={tokenConfirmOpen}'), 'token confirmation is not bound to a dialog');
ok(ui.includes('<AlertDialog.Cancel>'), 'token confirmation has no cancel action');
ok(ui.includes('<AlertDialog.Action onclick={regenerateToken}>'), 'regenerate is not gated by the dialog action');
ok(!/onclick=\{regenerateToken\}/.test(ui.replace('<AlertDialog.Action onclick={regenerateToken}>', '')), 'regenerate is reachable without confirmation');
ok(ui.includes('tokenConfirmOpen = false;'), 'successful regeneration does not close the dialog');
ok(ui.includes('{#if tokenError}'), 'regeneration errors are not shown inline');

// --- details folded behind their switches ------------------------------------------------------
const storeSource = readFileSync(store, 'utf8');
ok(ui.includes("from '$lib/integrations.svelte'"), 'the settings tab uses the shared store');
ok(ui.includes('syncIntegrations(loaded)'), 'the initial load syncs the shared store');
ok(ui.includes('syncIntegrations(next)'), 'an apply syncs the shared store');
ok(storeSource.includes('setIntegrationEnabled'), 'the shared store can toggle control/Tuna');
ok(storeSource.includes('setDiscordEnabled'), 'the shared store can toggle Discord');
ok(
	storeSource.includes('export async function setSharedControlEnabled'),
	'the shared store can toggle the host opt-in without clobbering other settings'
);
ok(
	storeSource.includes('export function discordEnabled()'),
	'Discord state is exported as a reader, not module-level derived state'
);
ok(
	!/export (?:const|let) \w+ = \$derived/.test(storeSource),
	'the shared store does not export derived state'
);

const enableRow = "title: t('settings.integrations.enable')";
const controlFold = ui.indexOf('{#if enabled}');
ok(controlFold > ui.indexOf(enableRow), 'control details follow their enable row');
ok(controlFold < ui.indexOf("title: t('settings.integrations.status')"), 'status is folded');
const tunaEnableRow = "title: t('settings.integrations.tuna_enable')";
const tunaFold = ui.indexOf('{#if tunaEnabled}');
ok(tunaFold > ui.indexOf(tunaEnableRow), 'Tuna details follow their enable row');
ok(tunaFold < ui.indexOf("title: t('settings.integrations.tuna_url')"), 'Tuna URL is folded');

ok(
	ui.includes('onCheckedChange={toggleSharedControl}') &&
		ui.includes('setSharedControlEnabled(on)'),
	'shared control is saved with Control API settings'
);
ok(ui.includes('settings?.control_api.shared_control'), 'the shared-control switch reads persisted state');
ok(ui.includes('setControl({ queue_dedupe_window })'), 'the queue window is saved live with Control API settings');
ok(
	readFileSync(join(here, 'api.ts'), 'utf8').includes('shared_control: boolean'),
	'the UI settings type carries shared_control'
);
ok(
	readFileSync(join(here, 'api.ts'), 'utf8').includes('queue_dedupe_window: number'),
	'the UI settings type carries queue_dedupe_window'
);

const folded: [string, string][] = [
	['settings.integrations.status', 'settings.integrations.enable'],
	['settings.integrations.shared_control', 'settings.integrations.enable'],
	['settings.integrations.queue_dedupe_window', 'settings.integrations.enable'],
	['settings.integrations.bind_mode', 'settings.integrations.enable'],
	['settings.integrations.port', 'settings.integrations.enable'],
	['settings.integrations.token', 'settings.integrations.enable'],
	['settings.integrations.connection', 'settings.integrations.enable'],
	['settings.integrations.clients', 'settings.integrations.enable'],
	['settings.integrations.status', 'settings.integrations.tuna_enable'],
	['settings.integrations.tuna_url', 'settings.integrations.tuna_enable'],
	['settings.integrations.tuna_test', 'settings.integrations.tuna_enable'],
	['settings.integrations.tuna_last_ok', 'settings.integrations.tuna_enable'],
	['settings.integrations.tuna_last_error', 'settings.integrations.tuna_enable'],
	['settings.integrations.tuna_backoff', 'settings.integrations.tuna_enable']
];
for (const [title, parent] of folded) {
	ok(
		SETTINGS.some((e) => e.tab === 'integrations' && e.title === title && e.under === parent),
		`${title} is indexed under ${parent}`
	);
}

// --- Discord lives in Integrations ------------------------------------------------------------
const dialogSource = readFileSync(dialog, 'utf8');
const discordSource = readFileSync(discord, 'utf8');
ok(!dialogSource.includes('DiscordSettings'), 'SettingsDialog no longer renders a Discord tab');
ok(!dialogSource.includes("id: 'discord'"), 'the Discord tab is gone from the tab rail');
ok(ui.includes("import DiscordSettings from '$lib/components/DiscordSettings.svelte';"), 'Integrations imports Discord');
ok(ui.includes('<DiscordSettings settings={appSettings} />'), 'Integrations renders Discord with the app settings');
ok(!SETTINGS.some((e) => e.tab === 'discord'), 'settings search has no removed Discord tab');

const discordEnableRow = "title: t('settings.discord.enable')";
const discordFold = discordSource.indexOf('{#if enabled}');
ok(discordFold > discordSource.indexOf(discordEnableRow), 'Discord details follow the enable row');
ok(
	discordSource.indexOf('data-settings-aside') > discordFold,
	'the live preview is folded with Discord'
);
ok(discordSource.includes('lg:flex-row'), 'the preview sits beside the controls on wide dialogs');
ok(!discordSource.includes('2xl:flex-row'), 'the preview is not stranded at an unreachable width');
ok(discordSource.includes('max-lg:border-t'), 'the stacked preview keeps its separator');
ok(dialogSource.includes('sm:max-w-5xl'), 'the Integrations dialog is wide enough for that split');
ok(
	discordSource.indexOf("title: t('settings.discord.show_paused')") > discordFold,
	'the card options are folded with Discord'
);

const discordRows = [
	'settings.discord.show_paused',
	'settings.discord.hide_details',
	'settings.discord.status_line',
	'settings.discord.app_name',
	'settings.discord.line1',
	'settings.discord.line2',
	'settings.discord.cover',
	'settings.discord.badge',
	'settings.discord.line3',
	'settings.discord.timestamps',
	'settings.discord.link_line1',
	'settings.discord.link_line2',
	'settings.discord.link_cover',
	'settings.discord.button1',
	'settings.discord.button2'
];
for (const title of discordRows) {
	ok(
		SETTINGS.some(
			(e) => e.tab === 'integrations' && e.title === title && e.under === 'settings.discord.enable'
		),
		`${title} is indexed under Discord's enable switch`
	);
}

// --- shared-control switch in the Listen Together Host setup ------------------------------------
const ltDialog = readFileSync(join(here, 'components', 'ListenTogether.svelte'), 'utf8');
const integrationStore = readFileSync(join(here, 'integrations.svelte.ts'), 'utf8');
const integrationSettings = readFileSync(join(here, 'components', 'IntegrationsSettings.svelte'), 'utf8');
ok(
	ltDialog.includes('setSharedControlEnabled(on)'),
	'the pre-host toggle saves only the opt-in'
);
ok(ltDialog.includes('controlOn && lt.sharedControlEnabled'), 'the switch reflects both gates');
ok(
	ltDialog.includes('disabled={!controlOn || sharedBusy}'),
	'the switch needs only the Control API'
);
ok(ltDialog.includes('void loadIntegrations()'), 'opening the dialog refreshes the API flag');
ok(
	ltDialog.includes('lt.sharedControlReason'),
	'the dialog explains why shared control is unavailable'
);
ok(
	integrationStore.includes('Leave Listen Together before changing shared control'),
	'both switches are locked while a room is active'
);
ok(
	!integrationStore.includes('BEFORE_SERVER_KEY') && !integrationStore.includes('ACTIVE_SERVER_KEY'),
	'no relay swap state remains'
);
ok(
	integrationSettings.includes('roomActive') &&
		integrationSettings.includes('settings?.control_api.shared_control'),
	'the settings toggle obeys the same idle rule'
);
ok(
	!readFileSync(join(here, 'api.ts'), 'utf8').includes('shared_control_server'),
	'the integration settings type no longer carries a relay URL'
);
ok(
	readFileSync(join(here, 'lt.svelte.ts'), 'utf8').includes('sharedControlReason: null'),
	'the reactive room state carries the diagnostic reason'
);
ok(
	readFileSync(join(here, 'api.ts'), 'utf8').includes('sharedControlReason: string | null'),
	'the LtState type carries the diagnostic reason'
);

// --- queue requester pills -------------------------------------------------------------------
const trackRowSource = readFileSync(join(here, 'components', 'TrackRow.svelte'), 'utf8');
const apiTypes = readFileSync(join(here, 'api.ts'), 'utf8');
ok(apiTypes.includes('submitted_by?: string'), 'queue rows carry a separate submitter');
ok(trackRowSource.includes('song.submitted_by'), 'queue rows render the submitter');
ok(trackRowSource.includes('song.queued_by'), 'queue rows render the visible requester');
ok(
	trackRowSource.indexOf('song.submitted_by') < trackRowSource.indexOf('song.queued_by'),
	'the submitter pill comes before the requester pill'
);

// --- titlebar integrations menu ---------------------------------------------------------------
const titlebarSource = readFileSync(titlebar, 'utf8');
ok(!titlebarSource.includes('toggleDiscord'), 'the direct Discord button is gone');
ok(!titlebarSource.includes('prefs.discordRpc'), 'the titlebar no longer owns the Discord flag');
ok(titlebarSource.includes('openIntegrationMenu'), 'the integrations menu has an opener');
ok(titlebarSource.includes('loadIntegrations'), 'the menu refreshes the shared store');
for (const kind of ['discord', 'control', 'tuna']) {
	ok(titlebarSource.includes(`toggleIntegration('${kind}')`), `the menu toggles ${kind}`);
}
ok(
	(titlebarSource.match(/role="menuitemcheckbox"/g) ?? []).length === 3,
	'the menu has exactly three toggle rows'
);
ok(
	titlebarSource.includes("ui.settingsFocus = 'integrations'"),
	'the menu can open the Integrations tab'
);
ok(
	dialogSource.includes("ui.settingsFocus === 'integrations'"),
	'SettingsDialog accepts the Integrations focus'
);

const en = JSON.parse(readFileSync(join(here, 'locales', 'en.json'), 'utf8'));
for (const key of [
	'menu_hint',
	'open_settings',
	'on',
	'off',
	'tooltip_on',
	'tooltip_off',
	'control_api_on',
	'control_api_off',
	'tuna_on',
	'tuna_off'
]) {
	ok(typeof en.integrations[key] === 'string', `en.json has integrations.${key}`);
}
for (const key of [
	'queue_dedupe_window',
	'queue_dedupe_window_hint',
	'queue_dedupe_window_invalid'
]) {
	ok(typeof en.settings.integrations[key] === 'string', `en.json has settings.integrations.${key}`);
}
for (const key of ['shared_control', 'shared_control_hint', 'shared_control_requires_api']) {
	ok(
		typeof en.dialogs.listen_together[key] === 'string',
		`en.json has dialogs.listen_together.${key}`
	);
}

console.log('ok');
