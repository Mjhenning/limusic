// Self-check for the fork's release formats: Windows NSIS, Linux AppImage, and macOS dmg/app.
// Node runs it with `npm test` (see run-checks.mjs). It reads the Tauri configs and release docs
// so a stray `msi`, `deb`, or `rpm` target cannot creep back into a build or a download table.
//
// @ts-nocheck: it reads files outside ui/ with node:fs, and ui/ has no Node types for svelte-check.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

function ok(cond: boolean, what: string): void {
	if (!cond) throw new Error(`FAIL: ${what}`);
}

function targets(file: string): string[] {
	const json = JSON.parse(readFileSync(file, 'utf8'));
	return json?.bundle?.targets ?? [];
}

function same(actual: string[], expected: string[], what: string): void {
	ok(
		actual.length === expected.length && actual.every((v, i) => v === expected[i]),
		`${what} targets are ${expected.join(' + ')} (got ${actual.join(' + ') || 'none'})`
	);
}

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..', '..');
const read = (file: string) => readFileSync(join(root, file), 'utf8');

same(targets(join(root, 'src-tauri', 'tauri.conf.json')), ['appimage'], 'Linux');
same(targets(join(root, 'src-tauri', 'tauri.windows.conf.json')), ['nsis'], 'Windows');
same(targets(join(root, 'src-tauri', 'tauri.macos.conf.json')), ['app', 'dmg'], 'macOS');

const base = JSON.parse(read('src-tauri/tauri.conf.json'));
ok(base.bundle.linux === undefined, 'no Linux package-manager bundle settings remain');

const docs = read('docs/BUILD-PLATFORMS.md');
for (const format of ['deb', 'rpm', 'msi']) {
	ok(!new RegExp(`--bundles\\s+${format}\\b`).test(docs), `the build guide has no --bundles ${format}`);
	ok(!docs.includes(`bundle/${format}/`), `the build guide has no ${format} output path`);
}

const readme = read('README.md');
ok(readme.includes('Only the three self-updating formats above are published here.'), 'README states the artifact policy');

console.log('ok');
