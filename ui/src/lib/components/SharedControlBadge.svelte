<script lang="ts">
	// A quiet, one-line answer to "can I control this room?" The full explanation lives in the
	// Listen Together dialog; this only tells the player why the buttons behave as they do. Feedback
	// from a guest button takes precedence so a press is acknowledged right where it happened.
	import { t } from '$lib/i18n.svelte';
	import { integrations } from '$lib/integrations.svelte';
	import { controlFeedback, inRoom, lt } from '$lib/lt.svelte';

	let { onArt = false }: { onArt?: boolean } = $props();

	const label = $derived.by(() => {
		if (controlFeedback.pending) return t('player.shared_sending');
		if (controlFeedback.ok === true) return t('player.shared_sent');
		if (controlFeedback.ok === false) return t('player.shared_failed');
		if (!inRoom() || lt.sharedControlReason !== null) return '';
		if (lt.role === 'host' && lt.sharedControlEnabled) return t('player.shared_host');
		if (lt.role === 'guest' && lt.hostSharedControlCapable && lt.sharedControl) {
			if (!integrations.loaded) return '';
			return integrations.clientControl
				? t('player.shared_guest')
				: t('player.shared_needs_client_control');
		}
		return '';
	});

	const tone = $derived(
		controlFeedback.ok === false
			? 'border-amber-500/50 text-amber-600 dark:text-amber-400'
			: controlFeedback.pending || controlFeedback.ok === true || lt.role === 'guest'
				? 'border-primary/40 text-primary'
				: 'border-border/70 text-muted-foreground'
	);
</script>

{#if label}
	<span
		data-shared-control-badge
		class="inline-flex shrink-0 items-center rounded-full border px-1.5 py-px text-[10px] font-medium leading-tight tracking-wide {onArt
			? 'border-white/25 bg-black/30 text-white/90'
			: tone}"
		title={lt.sharedControlReason ?? undefined}
	>
		{label}
	</span>
{/if}
