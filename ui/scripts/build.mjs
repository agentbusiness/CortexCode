// Builds every UI package, each on its own: the SDK, then every UI plugin package (`npm run build`
// in its folder, wherever its plugin folder is; the window is the workbench plugin's), and installs
// the builds into ui/plugins-dist (the assembled set; the frame serves plugin folders directly).
//
// Each plugin's ui/ is its own npm root (own package-lock.json and node_modules) holding a copy of
// @cortex/sdk (install-links). Before plugin packages build, missing installs are made with npm ci
// and every root's SDK copy is refreshed from the SDK just built, so no root builds against a
// stale SDK.
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { installAll } from './install-plugins.mjs';
import { SDK_DIR, npmRoots, uiPackages } from './packages.mjs';

const quiet = process.argv.includes('--quiet');

export function npmRun(dir, script) {
  return new Promise((resolve, reject) => {
    // A child decides its own mode (Vite sets NODE_ENV=production in this process when it builds).
    // The SDK is built first (CORTEX_SDK_BUILT): a package that would build it itself (the window) does not.
    const env = { ...process.env, CORTEX_SDK_BUILT: '1' };
    delete env.NODE_ENV;
    const child = spawn('npm', ['run', script, '--silent'], { cwd: dir, stdio: ['ignore', 'pipe', 'pipe'], env });
    let output = '';
    child.stdout.on('data', (d) => { output += d; });
    child.stderr.on('data', (d) => { output += d; });
    child.on('exit', (code) => (code === 0 ? resolve(output) : reject(new Error(`npm run ${script} in ${dir} failed (${code}):\n${output}`))));
  });
}

/** Run `work` over `items` with up to `limit` at a time. */
export async function pool(items, limit, work) {
  const results = [];
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (next < items.length) {
      const index = next++;
      results[index] = await work(items[index]);
    }
  }));
  return results;
}

function childEnv() {
  const env = { ...process.env, npm_config_audit: 'false', npm_config_fund: 'false', npm_config_update_notifier: 'false' };
  delete env.NODE_ENV;
  return env;
}

/** `npm ci` in a plugin's npm root when it has no install yet (offline-first from the npm cache). */
export function ensureInstalled(root) {
  if (fs.existsSync(path.join(root, 'node_modules', '.package-lock.json'))) return false;
  const result = spawnSync('npm', ['ci', '--prefer-offline'], { cwd: root, encoding: 'utf8', env: childEnv() });
  if (result.status !== 0) throw new Error(`npm ci in ${root} failed (${result.status}):\n${result.stdout}${result.stderr}`);
  return true;
}

/** The files `npm pack` would put in the SDK's tarball (what a root's installed copy holds). */
function sdkPackedFiles() {
  const result = spawnSync('npm', ['pack', '--dry-run', '--json', '--ignore-scripts'], { cwd: SDK_DIR, encoding: 'utf8', env: childEnv() });
  if (result.status !== 0) throw new Error(`npm pack --dry-run in the SDK failed:\n${result.stderr}`);
  return JSON.parse(result.stdout)[0].files.map((file) => file.path);
}

/** Replace a root's installed @cortex/sdk with the SDK's current packed files. */
export function refreshSdkCopy(root, files = sdkPackedFiles()) {
  const target = path.join(root, 'node_modules', '@cortex', 'sdk');
  if (!fs.existsSync(target) || fs.lstatSync(target).isSymbolicLink()) return;
  fs.rmSync(target, { recursive: true, force: true });
  for (const file of files) {
    fs.mkdirSync(path.dirname(path.join(target, file)), { recursive: true });
    fs.copyFileSync(path.join(SDK_DIR, file), path.join(target, file));
  }
}

/** Every plugin npm root installed, each with a fresh copy of the SDK. Call after the SDK build. */
export function prepareRoots() {
  const files = sdkPackedFiles();
  for (const root of npmRoots()) {
    ensureInstalled(root);
    refreshSdkCopy(root, files);
  }
}

export async function buildAll() {
  await npmRun(SDK_DIR, 'build');
  if (!quiet) console.log('built @cortex/sdk');
  prepareRoots();
  const packages = uiPackages();
  const failures = [];
  await pool(packages, Math.max(2, Math.floor(os.availableParallelism() / 2)), async (pkg) => {
    try {
      await npmRun(pkg.dir, 'build');
      if (!quiet) console.log(`built ${pkg.name} (${pkg.slot})`);
    } catch (error) {
      failures.push(error.message);
    }
  });
  if (failures.length) throw new Error(failures.join('\n\n'));
  return installAll();
}

if (import.meta.url === `file://${process.argv[1]}`) {
  buildAll().then((installed) => console.log(`${installed.length} UI plugins installed`)).catch((error) => {
    console.error(error.message);
    process.exit(1);
  });
}
