// Self-check for the fork's self-updates. Node runs it with `npm test` (see run-checks.mjs).
// It keeps the Rust kill switch, both manifest URLs, the signing key and the UI download page on
// this fork. A build that silently points at upstream, or a placeholder key that can verify nothing,
// is worse than no updater at all.
//
// @ts-nocheck: it reads files outside ui/ with node:fs, and ui/ has no Node types for svelte-check.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

function ok(cond: boolean, what: string): void {
	if (!cond) throw new Error(`FAIL: ${what}`);
}

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..', '..');
const read = (file: string) => readFileSync(join(root, file), 'utf8');

const OWNER = 'Mjhenning/limusic';
const STABLE = `https://github.com/${OWNER}/releases/latest/download/latest.json`;
const BETA = `https://github.com/${OWNER}/releases/download/beta/latest.json`;
const PUBLIC_KEY =
	'dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDZCQThEQ0Y0MUFEMTQ4MkQKUldRdFNORWE5TnlvYXl3K2ZzakFoRm05RjBlUmFkRGdQYU5WNWpxSmxkc0tVaTlqY2xQVU9oY1kK';

const conf = JSON.parse(read('src-tauri/tauri.conf.json'));
ok(conf.bundle.createUpdaterArtifacts === true, 'signed updater artifacts are built');
ok(conf.plugins.updater.endpoints.length === 1, 'one stable updater endpoint');
ok(conf.plugins.updater.endpoints[0] === STABLE, 'stable manifest comes from the fork');
ok(conf.plugins.updater.pubkey === PUBLIC_KEY, 'the fork public key is embedded');
ok(!conf.plugins.updater.pubkey.includes('MDAwMDAw'), 'no all-zero placeholder key remains');

const commands = read('src-tauri/src/commands.rs');
ok(commands.includes('pub(crate) const UPDATES_DISABLED: bool = false;'), 'updater kill switch is off');
// rustfmt wraps this long constant, so match the declaration and URL separately.
ok(commands.includes('const BETA_MANIFEST: &str ='), 'the beta manifest constant exists');
ok(commands.includes(`"${BETA}"`), 'beta manifest comes from the fork');
ok(!commands.includes('SimoHypers/limusic'), 'no upstream manifest URL remains');
ok(!commands.includes('limusic-updates-disabled'), 'no discard endpoint remains');

const lib = read('src-tauri/src/lib.rs');
ok(lib.includes('tauri_plugin_updater::Builder::new().build()'), 'the updater plugin is registered');
ok(lib.includes('if !commands::UPDATES_DISABLED'), 'the kill switch still gates registration');

const updater = read('ui/src/lib/updater.svelte.ts');
ok(updater.includes(`'https://github.com/${OWNER}/releases/latest'`), 'download page is the fork release');
ok(updater.includes('if (updateFlags.disabled) return false;'), 'quiet checks respect the kill switch');
ok(updater.includes('if (updateFlags.disabled || !pending) return;'), 'installs respect the kill switch');

console.log('ok');
