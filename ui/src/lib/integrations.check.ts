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
const root = join(here, '..', '..', '..');
const files = [
	component,
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

console.log('ok');
