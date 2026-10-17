// `npm run verify`: typecheck, test and build the settings panel, check the built bundle (manifest,
// externals, files), and run the shell module's tests. Prints NOTIFY_UI_OK only when every step
// passed; exits nonzero otherwise.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const UI = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SHELL = path.resolve(UI, '../shell');

function step(name, command, args, cwd) {
  const started = Date.now();
  const result = spawnSync(command, args, { cwd, stdio: 'inherit', env: process.env });
  if (result.status !== 0) {
    console.error(`\nverify: ${name} failed (exit ${result.status ?? result.signal})`);
    process.exit(1);
  }
  console.log(`verify: ${name} ok (${Date.now() - started} ms)`);
}

function check(condition, message) {
  if (!condition) {
    console.error(`verify: ${message}`);
    process.exit(1);
  }
}

const npx = process.platform === 'win32' ? 'npx.cmd' : 'npx';
// A fresh checkout has no node_modules yet: install exactly what the lockfile says.
if (!fs.existsSync(path.join(UI, 'node_modules/.package-lock.json'))) {
  step('install (npm ci)', process.platform === 'win32' ? 'npm.cmd' : 'npm', ['ci', '--no-audit', '--no-fund'], UI);
}
step('typecheck', npx, ['tsc', '-p', 'tsconfig.json'], UI);
step('typecheck tests', npx, ['tsc', '-p', 'tsconfig.test.json'], UI);
step('panel tests', npx, ['vitest', 'run'], UI);
step('build', npx, ['vite', 'build'], UI);

const dist = path.join(UI, 'dist');
const manifest = JSON.parse(fs.readFileSync(path.join(dist, 'manifest.json'), 'utf8'));
const pkg = JSON.parse(fs.readFileSync(path.join(UI, 'package.json'), 'utf8'));
check(manifest.id === pkg.cortex.id && manifest.id === 'notify.settings', `manifest id is ${manifest.id}`);
check(manifest.version === pkg.version, `manifest version ${manifest.version} != package ${pkg.version}`);
check(manifest.slot === 'settings.notify@1', `manifest slot is ${manifest.slot}`);
check(manifest.entry === 'index.js' && fs.existsSync(path.join(dist, manifest.entry)), 'manifest entry exists');
check(typeof manifest.css === 'string' && fs.existsSync(path.join(dist, manifest.css)), 'manifest css exists');
const bundle = fs.readFileSync(path.join(dist, manifest.entry), 'utf8');
// The workbench provides these through its import map; a bundled copy would be a second React.
for (const shared of ['react', '@cortex/sdk']) {
  check(new RegExp(`from\\s*["']${shared.replace('/', '\\/')}["']`).test(bundle), `the bundle imports ${shared} instead of bundling it`);
}
check(!/react-dom\.production|__SECRET_INTERNALS|ReactCurrentDispatcher/.test(bundle), 'React is not bundled');
check(bundle.includes('settings.notify@1'), 'the bundle declares its slot');
const css = fs.readFileSync(path.join(dist, manifest.css), 'utf8');
check(css.includes('.cx-notify') && !/#[0-9a-fA-F]{6}\b/.test(css), 'the stylesheet is the panel’s and uses tokens only');
console.log(`verify: bundle ok (${(bundle.length / 1024).toFixed(1)} KiB js, ${(css.length / 1024).toFixed(1)} KiB css)`);

step('shell module syntax', process.execPath, ['--check', path.join(SHELL, 'index.js')], SHELL);
step('shell module tests', process.execPath, ['--test', ...fs.readdirSync(path.join(SHELL, 'test')).filter((f) => f.endsWith('.test.mjs')).map((f) => path.join('test', f))], SHELL);

console.log('NOTIFY_UI_OK');
