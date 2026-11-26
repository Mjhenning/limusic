// Self-check for the Control API contract doc. `npm test` runs every *.check.ts,
// so a route added in Rust without a matching call-index row fails the UI suite.
//
//     node --experimental-strip-types ui/src/lib/control-api-doc.check.ts
//
// It also keeps the WebSocket close-code section aligned with the code that
// actually sends close frames.
//
// @ts-nocheck: it reads the sources with node:fs, and ui/ has no Node types.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

function ok(cond: boolean, what: string): void {
	if (!cond) throw new Error(`FAIL: ${what}`);
}

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..', '..');
const read = (path: string) => readFileSync(path, 'utf8');

const doc = read(join(root, 'docs', 'CONTROL-API.md'));
const handlers = read(join(root, 'src-tauri', 'src', 'control_api', 'handlers.rs'));
const server = read(join(root, 'src-tauri', 'src', 'control_api', 'server.rs'));
const ws = read(join(root, 'src-tauri', 'src', 'control_api', 'ws.rs'));
const protocol = read(join(root, 'crates', 'listen-protocol', 'src', 'lib.rs'));
const sync = read(join(root, 'crates', 'sync-server', 'src', 'main.rs'));

// --- call index vs. axum routes ---------------------------------------------------------------
const start = doc.indexOf('### Call index');
const end = doc.indexOf('### Read state', start);
ok(start >= 0 && end > start, 'CONTROL-API.md has a Call index section');
const section = doc.slice(start, end);

const routes = new Set(
	[...handlers.matchAll(/\.route\("(\/api\/v1\/[^"]+)",\s*(get|post)\(/g)].map(
		(m) => `${m[2]!.toUpperCase()} ${m[1]}`
	)
);
const documented = new Set(
	[...section.matchAll(/^\| `((?:GET|POST) \/api\/v1\/[^`]+)` \|/gm)].map((m) => m[1]!)
);

ok(routes.size === 16, `handlers.rs registers 16 routes, found ${routes.size}`);
ok(documented.size === routes.size, 'the call index has one row per registered route');
for (const route of routes) ok(documented.has(route), `call index documents ${route}`);
for (const route of documented) ok(routes.has(route), `call index row ${route} is a real route`);
ok(documented.has('GET /api/v1/ws'), 'the WebSocket upgrade route is in the call index');

// --- close frame semantics --------------------------------------------------------------------
const closeStart = doc.indexOf('#### Close frames');
const closeEnd = doc.indexOf('#### ', closeStart + 1);
ok(closeStart >= 0 && closeEnd > closeStart, 'CONTROL-API.md has a Close frames section');
const closeSection = doc.slice(closeStart, closeEnd);

const sources = `${server}\n${handlers}\n${ws}`;
const sourceReasons = new Set([
	'control API token regenerated',
	'control API disabled',
	'control API port changed',
	'control API settings changed',
	'control API shutting down',
	'control API stopped'
]);
const documentedReasons = new Set(
	[...closeSection.matchAll(/^\| `(control API [^`]+)` \|/gm)].map((m) => m[1]!)
);

for (const reason of sourceReasons) {
	ok(sources.includes(`"${reason}"`), `${reason} is present in the Rust sources`);
	ok(documentedReasons.has(reason), `close frames documents ${reason}`);
}
for (const reason of documentedReasons) {
	ok(sourceReasons.has(reason), `documented close reason ${reason} is real`);
}
for (const match of sources.matchAll(/(?:close_all|stop_control_api)\("([^"]+)"\)/g)) {
	ok(sourceReasons.has(match[1]!), `sent close reason ${match[1]} is documented`);
}
ok(ws.includes('code: 1001'), 'ws.rs sends close code 1001');
ok(closeSection.includes('1001'), 'the close-frame section names code 1001');

// --- optional shared Listen Together control --------------------------------------------------
const sharedStart = doc.indexOf('## Shared Listen Together control');
const sharedEnd = doc.indexOf('## Examples', sharedStart);
ok(sharedStart >= 0 && sharedEnd > sharedStart, 'CONTROL-API.md documents shared control');
const sharedSection = doc.slice(sharedStart, sharedEnd);
for (const text of [
	'listen_together_guest',
	'shared_control_failed',
	'15 s',
	'base clients',
	'/queue/request',
	'sync.limusic.app',
	'not advertise shared-control capability',
	'the first missing fact'
]) {
	ok(sharedSection.includes(text), `shared-control docs mention ${text}`);
}
const types = read(join(root, 'src-tauri', 'src', 'control_api', 'types.rs'));
const lt = read(join(root, 'src-tauri', 'src', 'listentogether', 'mod.rs'));
ok(types.includes('listen_together_guest_with'), 'guest errors carry the room blocker');
ok(lt.includes('sharedControlReason'), 'the room snapshot carries the diagnostic reason');
ok(handlers.includes('forward_shared_control'), 'handlers forward opted-in guest mutations');
ok(protocol.includes('SharedControlEnvelope'), 'the wire shape is in listen-protocol');
ok(sync.includes('MAX_SHARED_CONTROL'), 'the relay bounds shared-control envelopes');
ok(doc.includes('| 502 | `shared_control_failed` |'), 'the error table has shared_control_failed');

// --- live queue request window -----------------------------------------------------------------
const queueStart = doc.indexOf('### Queue requests');
const queueEnd = doc.indexOf('### WebSocket', queueStart);
ok(queueStart >= 0 && queueEnd > queueStart, 'CONTROL-API.md documents queue requests');
const queueSection = doc.slice(queueStart, queueEnd);
for (const text of [
	'queue_dedupe_window',
	'default `10`',
	'0 to 50',
	'claimed',
	'counterpart',
	'allow_duplicates'
]) {
	ok(queueSection.includes(text), `queue request docs mention ${text}`);
}
const state = read(join(root, 'src-tauri', 'src', 'state.rs'));
const settings = read(join(root, 'src-tauri', 'src', 'integrations', 'settings.rs'));
ok(settings.includes('queue_dedupe_window: usize'), 'the setting is persisted in Rust');
ok(state.includes('pub fn queue_dedupe_window'), 'the window is read live per request');
ok(state.includes('apply_shared_queue_action'), 'guests use the same host-side policy');
ok(doc.includes('host applies its own'), 'shared-control docs name the host window');

console.log('ok');
