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

const linux = read('.github/workflows/linux-release.yml');
const windows = read('.github/workflows/windows-release.yml');
const macos = read('.github/workflows/macos-release.yml');
const script = read('scripts/release.sh');

ok(linux.includes('tauri build --bundles appimage'), 'Linux CI builds only the AppImage');
ok(windows.includes('tauri build --bundles nsis'), 'Windows CI builds only NSIS');
ok(macos.includes('tauri build --bundles app'), 'macOS CI builds the .app before dmg/tarball');
ok(macos.includes('.app.tar.gz') && macos.includes('.dmg'), 'macOS CI ships the updater tarball and dmg');

for (const [name, source] of [
	['Linux CI', linux],
	['Windows CI', windows],
	['release script', script]
]) {
	ok(!/\b(?:deb|rpm|msi)\b/i.test(source), `${name} has no dropped format`);
	ok(!source.includes('bundle/deb') && !source.includes('bundle/rpm') && !source.includes('bundle/msi'), `${name} attaches no dropped artifact`);
}
ok(!linux.includes('deb_check'), 'the Linux .deb verification step is gone');
ok(!script.includes('--bundles rpm'), 'the release script no longer builds an rpm');

for (const [name, source] of [
	['Linux', linux],
	['Windows', windows],
	['macOS', macos]
]) {
	ok(
		source.includes('REQUIRED_PLATFORMS=\'["darwin-aarch64","linux-x86_64","windows-x86_64"]\''),
		`${name} still gates promotion on all three platforms`
	);
}

console.log('ok');
