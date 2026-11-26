// Reactive Listen Together state, driven by the Rust `lt-state` event. The modal + home-header
// button read this; mutations go through `api` commands. context/19.
import type { ControlFeedback, LtState } from './api';

export const lt = $state<LtState>({
	status: 'disconnected',
	role: 'none',
	requesting: false,
	roomCode: null,
	myId: null,
	serverUrl: '',
	defaultServerUrl: '',
	users: [],
	currentTrack: null,
	queue: [],
	pendingJoins: [],
	suggestions: [],
	sharedControlEnabled: false,
	sharedControlLinkReady: false,
	sharedControlLinkGuests: 0,
	sharedControlGuests: [],
	sharedControlCapable: false,
	sharedControl: false,
	hostSharedControlCapable: false,
	sharedControlReason: null
});

/** True when we're in (or joining) a room. */
export function inRoom(): boolean {
	return lt.role !== 'none';
}

/** Replace the reactive state from a fresh `lt-state` snapshot. */
export function applyLtState(s: LtState) {
	Object.assign(lt, s);
}

/** Transient delivery state for a guest's forwarded transport button. */
export const controlFeedback = $state<ControlFeedback>({
	pending: false,
	ok: null,
	message: ''
});
let feedbackTimer: ReturnType<typeof setTimeout> | undefined;

export function applyControlFeedback(f: ControlFeedback) {
	Object.assign(controlFeedback, f);
	if (feedbackTimer) clearTimeout(feedbackTimer);
	feedbackTimer = f.pending
		? undefined
		: setTimeout(() => {
				controlFeedback.message = '';
				controlFeedback.ok = null;
			}, 2200);
}
