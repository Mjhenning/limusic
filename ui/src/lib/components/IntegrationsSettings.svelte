<script lang="ts">
	// Settings tab for the fork's optional outputs. Backend state is already live: applying a change
	// restarts the listener or Tuna worker immediately, so this page only owns the form and a 2 s
	// status poll while it is actually visible.
	import { untrack, type Snippet } from 'svelte';
	import { Button } from '$lib/components/ui/button';
	import * as AlertDialog from '$lib/components/ui/alert-dialog';
	import { Input } from '$lib/components/ui/input';
	import { Switch } from '$lib/components/ui/switch';
	import * as Select from '$lib/components/ui/select';
	import * as api from '$lib/api';
	import DiscordSettings from '$lib/components/DiscordSettings.svelte';
	import { copyText } from '$lib/clipboard';
	import { integrations, syncIntegrations } from '$lib/integrations.svelte';
	import { toast, ui } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';

	const GROUP = 'mb-7 last:mb-1';
	const LABEL =
		'mb-2 px-1 text-[11px] font-semibold uppercase tracking-[0.08em] text-muted-foreground';
	const CARD = 'divide-y divide-border/60 overflow-hidden rounded-xl border bg-card';

	let { settings: appSettings }: { settings: Record<string, string> } = $props();

	let settings = $state<api.IntegrationSettings | null>(null);
	let status = $state<api.IntegrationStatus | null>(null);
	let token = $state('');
	let localIps = $state<string[]>([]);
	let showToken = $state(false);
	let tokenConfirmOpen = $state(false);
	let tokenError = $state('');
	let applying = $state(false);
	let error = $state('');
	let portInput = $state('27199');
	let urlInput = $state('http://127.0.0.1:1608/');
	let testMessage = $state<{ ok: boolean; text: string } | null>(null);

	const enabled = $derived(integrations.controlApi);
	const bindValue = $derived(settings?.control_api.bind ?? '127.0.0.1');
	const tunaEnabled = $derived(integrations.tuna);
	const isLocal = $derived(bindValue !== '127.0.0.1');

	const portValid = $derived.by(() => {
		const port = Number(portInput);
		return Number.isInteger(port) && port >= 1024 && port <= 65535;
	});
	const urlValid = $derived(/^https?:\/\/\S+/i.test(urlInput.trim()));

	const addresses = $derived.by(() => {
		if (!settings) return [] as string[];
		const { bind, port } = settings.control_api;
		if (bind === '127.0.0.1') return [`http://127.0.0.1:${port}`];
		const hosts = localIps.length ? localIps : [bind];
		return hosts.map((host) => `http://${host}:${port}`);
	});

	const controlBadge = $derived.by(() => {
		const s = status?.control_api;
		if (!s) return { text: t('common.loading'), bad: false };
		if (s.last_error) {
			return { text: t('settings.integrations.status_error', { reason: s.last_error }), bad: true };
		}
		return {
			text: s.running ? t('settings.integrations.status_running') : t('settings.integrations.status_stopped'),
			bad: false
		};
	});

	const tunaBadge = $derived.by(() => {
		const s = status?.tuna;
		if (!s) return { text: t('common.loading'), bad: false };
		if (s.last_error) {
			return { text: t('settings.integrations.status_error', { reason: s.last_error }), bad: true };
		}
		return {
			text: s.enabled ? t('settings.integrations.status_running') : t('settings.integrations.status_stopped'),
			bad: false
		};
	});

	function badgeClass(bad: boolean): string {
		return bad
			? 'rounded-full bg-destructive/12 px-1.5 py-0.5 text-[10px] font-semibold text-destructive'
			: 'rounded-full bg-primary/12 px-1.5 py-0.5 text-[10px] font-semibold text-primary';
	}

	function formatTime(ms: number | null): string {
		return ms ? new Date(ms).toLocaleString() : t('settings.integrations.never');
	}

	async function refreshStatus() {
		try {
			status = await api.getIntegrationStatus();
		} catch (e) {
			error = String(e);
		}
	}

	async function load() {
		error = '';
		try {
			const [loaded, currentStatus, currentToken] = await Promise.all([
				api.getIntegrationSettings(),
				api.getIntegrationStatus(),
				api.getControlApiToken()
			]);
			settings = loaded;
			syncIntegrations(loaded);
			status = currentStatus;
			token = currentToken;
			portInput = String(loaded.control_api.port);
			urlInput = loaded.tuna.url;
			api.getLocalIpv4Addresses().then((ips) => (localIps = ips)).catch(() => {});
		} catch (e) {
			error = String(e);
		}
	}

	$effect(() => {
		if (!ui.settingsOpen) return;
		untrack(() => {
			void load();
		});
	});

	$effect(() => {
		if (!ui.settingsOpen || !settings) return;
		let stopped = false;
		void refreshStatus();
		const timer = setInterval(() => {
			if (!stopped) void refreshStatus();
		}, 2000);
		return () => {
			stopped = true;
			clearInterval(timer);
		};
	});

	async function apply(next: api.IntegrationSettings) {
		if (applying) return;
		if (!portValid || !urlValid) return;
		error = '';
		applying = true;
		try {
			await api.setIntegrationSettings(next);
			settings = next;
			syncIntegrations(next);
			portInput = String(next.control_api.port);
			urlInput = next.tuna.url;
			await refreshStatus();
		} catch (e) {
			error = t('settings.integrations.apply_error', { reason: String(e) });
		} finally {
			applying = false;
		}
	}

	function setControl(patch: Partial<api.ControlApiSettings>) {
		if (!settings) return;
		void apply({
			...settings,
			control_api: { ...settings.control_api, ...patch }
		});
	}

	function setTuna(patch: Partial<api.TunaSettings>) {
		if (!settings) return;
		void apply({
			...settings,
			tuna: { ...settings.tuna, ...patch }
		});
	}

	function savePort() {
		const port = Number(portInput);
		if (!Number.isInteger(port)) return;
		setControl({ port });
	}

	function saveUrl() {
		setTuna({ url: urlInput.trim() });
	}

	async function copyToken() {
		try {
			await copyText(token);
			toast.success(t('settings.integrations.token_copied'));
		} catch (e) {
			toast.error(String(e));
		}
	}

	async function regenerateToken() {
		applying = true;
		tokenError = '';
		try {
			token = await api.regenerateControlApiToken();
			showToken = false;
			await refreshStatus();
			tokenConfirmOpen = false;
		} catch (e) {
			// Keep the dialog open: the error belongs next to the action the user just took.
			tokenError = t('settings.integrations.apply_error', { reason: String(e) });
		} finally {
			applying = false;
		}
	}

	async function testPush() {
		testMessage = null;
		applying = true;
		try {
			const next = await api.tunaTestPush();
			status = status ? { ...status, tuna: next } : status;
			testMessage = next.last_error
				? { ok: false, text: t('settings.integrations.tuna_test_fail', { reason: next.last_error }) }
				: { ok: true, text: t('settings.integrations.tuna_test_ok') };
		} catch (e) {
			testMessage = { ok: false, text: t('settings.integrations.tuna_test_fail', { reason: String(e) }) };
		} finally {
			applying = false;
		}
	}
</script>

{#snippet row(o: { title: string; desc?: string; control?: Snippet; below?: Snippet })}
	<div data-setting-row class="px-4 py-3.5">
		<div class="flex items-center justify-between gap-4">
			<span class="min-w-0 text-sm font-medium">{o.title}</span>
			{#if o.control}
				<div class="shrink-0">{@render o.control()}</div>
			{/if}
		</div>
		{#if o.desc}
			<p class="mt-1.5 max-w-prose text-xs leading-relaxed text-muted-foreground">{o.desc}</p>
		{/if}
		{#if o.below}
			<div class="mt-3">{@render o.below()}</div>
		{/if}
	</div>
{/snippet}

<div class="min-w-0 flex-1 overflow-y-auto overflow-x-hidden px-6 py-5">
	{#if error}
		<p class="mb-4 text-xs text-destructive">{error}</p>
	{/if}

	<section class={GROUP}>
		<h3 class={LABEL}>{t('settings.integrations.control_api')}</h3>
		<p class="mb-2 px-1 text-xs text-muted-foreground">{t('settings.integrations.control_api_desc')}</p>
		<div data-settings-card class={CARD}>
			{@render row({
				title: t('settings.integrations.enable'),
				desc: t('settings.integrations.enable_hint'),
				control: controlSwitch
			})}
			{#if enabled}
				{@render row({
					title: t('settings.integrations.status'),
					control: controlStatus
				})}
				{@render row({
					title: t('settings.integrations.bind_mode'),
					desc: t('settings.integrations.bind_mode_hint'),
					control: bindPicker,
					below: localWarning
				})}
				{@render row({
					title: t('settings.integrations.port'),
					desc: t('settings.integrations.port_hint'),
					control: portField
				})}
				{@render row({
					title: t('settings.integrations.token'),
					desc: t('settings.integrations.token_hint'),
					control: tokenField
				})}
				{@render row({
					title: t('settings.integrations.connection'),
					desc: t('settings.integrations.connection_hint'),
					below: connectionList
				})}
				{@render row({
					title: t('settings.integrations.clients'),
					control: clientCount
				})}
			{/if}
		</div>
	</section>

	<section class={GROUP}>
		<h3 class={LABEL}>{t('settings.integrations.tuna')}</h3>
		<p class="mb-2 px-1 text-xs text-muted-foreground">{t('settings.integrations.tuna_desc')}</p>
		<div data-settings-card class={CARD}>
			{@render row({
				title: t('settings.integrations.tuna_enable'),
				desc: t('settings.integrations.tuna_enable_hint'),
				control: tunaSwitch
			})}
			{#if tunaEnabled}
				{@render row({
					title: t('settings.integrations.status'),
					control: tunaStatusBadge
				})}
				{@render row({
					title: t('settings.integrations.tuna_url'),
					desc: t('settings.integrations.tuna_url_hint'),
					control: urlField
				})}
				{@render row({
					title: t('settings.integrations.tuna_test'),
					control: testButton,
					below: testResult
				})}
				{@render row({
					title: t('settings.integrations.tuna_last_ok'),
					control: lastOk
				})}
				{@render row({
					title: t('settings.integrations.tuna_last_error'),
					control: lastError
				})}
				{@render row({
					title: t('settings.integrations.tuna_backoff'),
					control: backoff
				})}
			{/if}
		</div>
	</section>

	<section class={GROUP}>
		<h3 class={LABEL}>{t('settings.tabs.discord')}</h3>
		<DiscordSettings settings={appSettings} />
	</section>
</div>

<AlertDialog.Root bind:open={tokenConfirmOpen}>
	<AlertDialog.Content>
		<AlertDialog.Header>
			<AlertDialog.Title>{t('settings.integrations.token_regenerate_title')}</AlertDialog.Title>
			<AlertDialog.Description>
				{t('settings.integrations.token_regenerate_confirm')}
			</AlertDialog.Description>
			{#if tokenError}
				<p class="px-4 pb-1 text-sm text-destructive">{tokenError}</p>
			{/if}
		</AlertDialog.Header>
		<AlertDialog.Footer>
			<AlertDialog.Cancel>{t('common.cancel')}</AlertDialog.Cancel>
			<AlertDialog.Action onclick={regenerateToken}>
				{t('settings.integrations.token_regenerate')}
			</AlertDialog.Action>
		</AlertDialog.Footer>
	</AlertDialog.Content>
</AlertDialog.Root>

{#snippet controlSwitch()}
	<Switch
		checked={enabled}
		disabled={applying || !settings}
		onCheckedChange={(on) => setControl({ enabled: on })}
	/>
{/snippet}

{#snippet controlStatus()}
	<span class={badgeClass(controlBadge.bad)}>{controlBadge.text}</span>
{/snippet}

{#snippet bindPicker()}
	<Select.Root
		type="single"
		value={bindValue}
		onValueChange={(v) => setControl({ bind: v })}
	>
		<Select.Trigger class="w-40 shrink-0" disabled={applying || !settings}>
			<span class="flex-1 truncate text-left">
				{isLocal ? t('settings.integrations.bind_local') : t('settings.integrations.bind_loopback')}
			</span>
		</Select.Trigger>
		<Select.Content>
			<Select.Item value="127.0.0.1" label={t('settings.integrations.bind_loopback')}>
				{t('settings.integrations.bind_loopback')}
			</Select.Item>
			<Select.Item value="0.0.0.0" label={t('settings.integrations.bind_local')}>
				{t('settings.integrations.bind_local')}
			</Select.Item>
		</Select.Content>
	</Select.Root>
{/snippet}

{#snippet localWarning()}
	{#if isLocal}
		<p class="rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs leading-relaxed text-amber-700 dark:text-amber-300">
			{t('settings.integrations.local_warning')}
		</p>
	{/if}
{/snippet}

{#snippet portField()}
	<div>
		<Input
			type="number"
			min="1024"
			max="65535"
			class="w-24"
			value={portInput}
			disabled={applying || !settings}
			oninput={(e) => (portInput = e.currentTarget.value)}
			onblur={savePort}
			aria-label={t('settings.integrations.port')}
		/>
		{#if !portValid}
			<p class="mt-1.5 text-xs text-destructive">{t('settings.integrations.port_invalid')}</p>
		{/if}
	</div>
{/snippet}

{#snippet tokenField()}
	<div class="flex items-center gap-1.5">
		<Input
			type={showToken ? 'text' : 'password'}
			class="w-44 font-mono text-xs"
			value={token}
			readonly
			aria-label={t('settings.integrations.token')}
		/>
		<Button variant="ghost" size="sm" onclick={() => (showToken = !showToken)}>
			{showToken ? t('settings.integrations.token_hide') : t('settings.integrations.token_show')}
		</Button>
		<Button variant="ghost" size="sm" disabled={applying} onclick={copyToken}>
			{t('settings.integrations.token_copy')}
		</Button>
		<Button
			variant="outline"
			size="sm"
			disabled={applying}
			onclick={() => {
				tokenError = '';
				tokenConfirmOpen = true;
			}}
		>
			{t('settings.integrations.token_regenerate')}
		</Button>
	</div>
{/snippet}

{#snippet connectionList()}
	<div class="space-y-1">
		{#each addresses as address (address)}
			<p class="font-mono text-xs text-muted-foreground">{address}</p>
		{/each}
	</div>
{/snippet}

{#snippet clientCount()}
	<span class="text-sm tabular-nums">{status?.control_api.ws_clients ?? 0}</span>
{/snippet}

{#snippet tunaSwitch()}
	<Switch
		checked={tunaEnabled}
		disabled={applying || !settings}
		onCheckedChange={(on) => setTuna({ enabled: on })}
	/>
{/snippet}

{#snippet tunaStatusBadge()}
	<span class={badgeClass(tunaBadge.bad)}>{tunaBadge.text}</span>
{/snippet}

{#snippet urlField()}
	<div>
		<Input
			type="url"
			class="w-56"
			value={urlInput}
			disabled={applying || !settings}
			oninput={(e) => (urlInput = e.currentTarget.value)}
			onblur={saveUrl}
			aria-label={t('settings.integrations.tuna_url')}
			spellcheck={false}
		/>
		{#if !urlValid}
			<p class="mt-1.5 text-xs text-destructive">{t('settings.integrations.tuna_url_invalid')}</p>
		{/if}
	</div>
{/snippet}

{#snippet testButton()}
	<Button variant="outline" size="sm" disabled={applying} onclick={testPush}>
		{t('settings.integrations.tuna_test')}
	</Button>
{/snippet}

{#snippet testResult()}
	{#if testMessage}
		<p class="text-xs {testMessage.ok ? 'text-emerald-600 dark:text-emerald-400' : 'text-destructive'}">
			{testMessage.text}
		</p>
	{/if}
{/snippet}

{#snippet lastOk()}
	<span class="text-xs text-muted-foreground">{formatTime(status?.tuna.last_ok_at_ms ?? null)}</span>
{/snippet}

{#snippet lastError()}
	<span class="text-xs text-muted-foreground">{status?.tuna.last_error ?? '—'}</span>
{/snippet}

{#snippet backoff()}
	<span class="text-xs text-muted-foreground">
		{status?.tuna.backoff_secs ? `${status.tuna.backoff_secs}s` : '—'}
	</span>
{/snippet}
