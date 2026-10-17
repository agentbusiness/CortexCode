// P2 (N5): every plugin folder, the frame, cx and the UI core build standalone, outside this
// repository, with cc-protocol from a separate git repository and @cortex/sdk from a packed
// tarball.
//
//   node scripts/split-check.mjs
//
// 1. protocol/ becomes a git repository of its own in a temporary directory (file:// URL), and the
//    SDK (ui/sdk) is built and packed with npm pack.
// 2. Every plugin folder with a plugin.toml, the frame, cli/ (cx) and the UI core (ui/, without
//    its node_modules and build outputs) is copied alone into its own directory, without
//    target/, node_modules/, dist/ or results; each becomes its own git repository.
// 3. In each copy the only dependencies that pointed into the monorepo are repointed, exactly as a
//    repository of its own would declare them: `cc-protocol = { path = ... }` becomes the git
//    repository, `@cortex/sdk: file:...` becomes the tarball. Any other path or file dependency
//    that leaves the copy fails the check (structural scan of Cargo.toml and package.json).
// 4. Each copy builds where it is: cargo build --release (backends, frame, cx; SDKROOT set to the
//    SDK the monorepo's .cargo/config.toml pins, this machine's linker workaround), npm install from the tarball + npm run build + npm test (UI
//    packages), node --test (shell modules), and the UI core's SDK, renderer build and type-check.
// 5. Each plugin copy is packed by cortex-pack into a .ccplugin that verifies, so the standalone
//    folder is a complete plugin.
// Prints "split check passed". Results: packer/results/split-check.json.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const RESULTS = path.join(REPO, 'packer', 'results');
const PACK = path.join(REPO, 'packer', 'target', 'release', 'cortex-pack');
const SKIP = new Set(['target', 'node_modules', 'dist', 'results', 'plugins-dist', '.git', 'screens']);

const log = (...args) => console.log(`[${new Date().toISOString().slice(11, 23)}]`, ...args);
const failures = [];
const rows = [];
let root = null;

function cleanup() {
  if (root && !process.env.KEEP_SPLIT) fs.rmSync(root, { recursive: true, force: true });
}
process.on('exit', cleanup);
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(130));

function env(extra = {}) {
  const value = { ...process.env, npm_config_audit: 'false', npm_config_fund: 'false', npm_config_update_notifier: 'false', ...extra };
  delete value.NODE_ENV;
  // The monorepo's cargo config must not leak in: the copies live outside it.
  delete value.CARGO_TARGET_DIR;
  return value;
}

function run(cmd, args, cwd, extra = {}) {
  const result = spawnSync(cmd, args, { cwd, env: env(extra), encoding: 'utf8', maxBuffer: 1 << 28 });
  return { ok: result.status === 0, out: `${result.stdout ?? ''}${result.stderr ?? ''}` };
}

function must(label, cmd, args, cwd, extra) {
  const started = Date.now();
  const result = run(cmd, args, cwd, extra);
  rows.push({ step: label, ok: result.ok, ms: Date.now() - started });
  log(`${label}: ${result.ok ? 'ok' : 'FAILED'} (${((Date.now() - started) / 1000).toFixed(1)} s)`);
  if (!result.ok) failures.push(`${label}:\n${result.out.split('\n').slice(-30).join('\n')}`);
  return result;
}

function fail(message) {
  failures.push(message);
  rows.push({ step: message, ok: false, ms: 0 });
  log(`FAILED: ${message}`);
}

function copy(from, to) {
  fs.cpSync(from, to, { recursive: true, filter: (file) => !SKIP.has(path.basename(file)) });
}

function gitRepo(dir, message) {
  run('git', ['init', '-q', '-b', 'main'], dir);
  run('git', ['add', '-A'], dir);
  const committed = run('git', ['-c', 'user.name=split-check', '-c', 'user.email=split@cortexcode.invalid', 'commit', '-q', '-m', message], dir);
  if (!committed.ok) throw new Error(`git commit in ${dir} failed: ${committed.out}`);
}

/** Manifests of the folder's own code (fixture repositories inside it are data, not code). */
function walk(dir, name, out = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (SKIP.has(entry.name) || entry.name === 'fixtures') continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(full, name, out);
    else if (entry.name === name) out.push(full);
  }
  return out;
}

/** Repoint cc-protocol to the git repository; any other path dependency leaving `top` fails. */
function rewriteCargo(top, protocolUrl) {
  for (const file of walk(top, 'Cargo.toml')) {
    let text = fs.readFileSync(file, 'utf8');
    text = text.replace(/cc-protocol\s*=\s*\{([^}]*)\}/g, (whole, inner) => {
      if (!/\bpath\s*=/.test(inner)) return whole;
      const rest = inner.replace(/\bpath\s*=\s*"[^"]*"\s*,?/, '').trim().replace(/,$/, '').trim();
      return `cc-protocol = { git = ${JSON.stringify(protocolUrl)}, branch = "main"${rest ? `, ${rest}` : ''} }`;
    });
    fs.writeFileSync(file, text);
    // Structural scan: every remaining `path = "..."` in a dependency table must stay inside.
    for (const match of text.matchAll(/\{[^}]*\bpath\s*=\s*"([^"]+)"[^}]*\}/g)) {
      const target = path.resolve(path.dirname(file), match[1]);
      if (!target.startsWith(top + path.sep) && target !== top) fail(`${path.relative(root, file)} depends on ${match[1]} outside its folder`);
    }
  }
}

/** Repoint @cortex/sdk to the tarball; any other file:/link: dependency leaving `top` fails. */
function rewriteNpm(top, tarball) {
  for (const file of walk(top, 'package.json')) {
    const pkg = JSON.parse(fs.readFileSync(file, 'utf8'));
    let changed = false;
    for (const field of ['dependencies', 'devDependencies', 'peerDependencies', 'optionalDependencies']) {
      for (const [name, spec] of Object.entries(pkg[field] ?? {})) {
        if (typeof spec !== 'string' || !/^(file|link):/.test(spec)) continue;
        if (name === '@cortex/sdk' && field !== 'peerDependencies') {
          pkg[field][name] = `file:${tarball}`;
          changed = true;
          continue;
        }
        const target = path.resolve(path.dirname(file), spec.replace(/^(file|link):/, ''));
        if (!target.startsWith(top + path.sep)) fail(`${path.relative(root, file)} depends on ${spec} outside its folder`);
      }
    }
    if (changed) fs.writeFileSync(file, JSON.stringify(pkg, null, 2) + '\n');
  }
  // The lockfiles name the old file: location; they are re-resolved by npm install.
}

function main() {
  root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'cortex-split-')));
  log(`split root ${root}`);
  const started = Date.now();
  // The linker workaround of this machine: the SDK the monorepo's .cargo/config.toml pins (the
  // copies live outside it, so it is passed explicitly); else what select-macos-sdk.sh finds.
  const pinned = fs.readFileSync(path.join(REPO, '.cargo', 'config.toml'), 'utf8').match(/^SDKROOT\s*=\s*"([^"]+)"/m)?.[1];
  let SDKROOT = pinned && fs.existsSync(pinned) ? pinned : null;
  if (!SDKROOT) {
    const selected = run('bash', [path.join(REPO, 'scripts', 'select-macos-sdk.sh')], REPO);
    if (!selected.ok) throw new Error(`no usable macOS SDK: ${selected.out}`);
    SDKROOT = selected.out.trim().split('\n').at(-1);
  }
  log(`SDKROOT ${SDKROOT}`);
  const cargoEnv = { SDKROOT };

  // 1. The protocol as its own repository; the SDK as a tarball.
  const protocolRepo = path.join(root, 'repos', 'cc-protocol');
  copy(path.join(REPO, 'protocol'), protocolRepo);
  gitRepo(protocolRepo, 'cc-protocol');
  const protocolUrl = `file://${protocolRepo}`;
  if (!must('ui/sdk: npm run build', 'npm', ['run', 'build', '--silent'], path.join(REPO, 'ui', 'sdk')).ok) return finish(started);
  const packed = run('npm', ['pack', '--json', '--pack-destination', root], path.join(REPO, 'ui', 'sdk'));
  if (!packed.ok) throw new Error(`npm pack failed: ${packed.out}`);
  const tarball = path.join(root, JSON.parse(packed.out.slice(packed.out.indexOf('[')))[0].filename);
  log(`@cortex/sdk tarball ${path.basename(tarball)}`);
  must('packer: cargo build --release', 'cargo', ['build', '--release'], path.join(REPO, 'packer'));

  // 2.–5. Every plugin folder.
  const plugins = fs.readdirSync(path.join(REPO, 'plugins')).filter((name) => fs.existsSync(path.join(REPO, 'plugins', name, 'plugin.toml'))).sort();
  const packedPlugins = [];
  for (const name of plugins) {
    const dir = path.join(root, 'split', name);
    copy(path.join(REPO, 'plugins', name), dir);
    rewriteCargo(dir, protocolUrl);
    rewriteNpm(dir, tarball);
    gitRepo(dir, name);
    const info = run(PACK, ['info', dir], dir);
    if (!info.ok) {
      fail(`${name}: plugin.toml: ${info.out}`);
      continue;
    }
    const toml = JSON.parse(info.out.slice(info.out.indexOf('{')));
    if (toml.backend) must(`${name}: cargo build --release (cc-protocol from git)`, 'cargo', ['build', '--release'], path.join(dir, toml.backend.dir ?? 'backend'), cargoEnv);
    if (toml.ui) {
      const uiRoot = path.join(dir, toml.ui.dir);
      if (must(`${name}: npm install (@cortex/sdk from the tarball)`, 'npm', ['install', '--prefer-offline', '--no-audit', '--no-fund'], uiRoot).ok) {
        const installed = JSON.parse(fs.readFileSync(path.join(uiRoot, 'node_modules', '@cortex', 'sdk', 'package.json'), 'utf8'));
        const lock = fs.readFileSync(path.join(uiRoot, 'package-lock.json'), 'utf8');
        if (!lock.includes(path.basename(tarball)) || installed.name !== '@cortex/sdk') fail(`${name}: @cortex/sdk was not installed from the tarball`);
        must(`${name}: npm run build`, 'npm', ['run', 'build'], uiRoot);
        must(`${name}: npm test`, 'npm', ['test'], uiRoot);
        for (const pkg of toml.ui.package) {
          const manifestFile = path.join(dir, pkg.dist ?? `${pkg.dir}/dist`, 'manifest.json');
          const manifest = fs.existsSync(manifestFile) ? JSON.parse(fs.readFileSync(manifestFile, 'utf8')) : null;
          if (!manifest || manifest.id !== pkg.id || manifest.slot !== pkg.slot) fail(`${name}: ${pkg.id} built no matching manifest.json`);
        }
      }
    }
    for (const shell of toml.shell ?? []) must(`${name}: shell module ${shell.id} tests`, 'npm', ['test'], path.join(dir, shell.dir));
    if (!toml.plugin.test_only) {
      const out = path.join(root, 'packs', `${name}.ccplugin`);
      if (must(`${name}: cortex-pack build (from the standalone folder)`, PACK, ['build', dir, '-o', out, '--no-build'], dir).ok) {
        const verified = must(`${name}: cortex-pack verify`, PACK, ['verify', out], dir);
        if (verified.ok) packedPlugins.push({ plugin: name, bytes: fs.statSync(out).size, summary: verified.out.trim() });
      }
    }
  }

  // The frame and cx.
  for (const [label, folder] of [['frame', 'frame'], ['cx', 'cli']]) {
    const dir = path.join(root, 'split', folder);
    copy(path.join(REPO, folder), dir);
    rewriteCargo(dir, protocolUrl);
    gitRepo(dir, label);
    must(`${label}: cargo build --release (cc-protocol from git)`, 'cargo', ['build', '--release'], dir, cargoEnv);
  }
  const frameBin = path.join(root, 'split', 'frame', 'target', 'release', 'cortex-frame');
  const version = run(frameBin, ['--version'], root);
  if (!version.ok || !version.out.includes('cortex-frame')) fail('the standalone frame binary does not run');

  // The UI core (renderer, SDK, shell, build and verify tooling).
  const uiDir = path.join(root, 'split', 'ui');
  copy(path.join(REPO, 'ui'), uiDir);
  rewriteNpm(uiDir, tarball);
  gitRepo(uiDir, 'ui');
  // Electron's binary is not needed to build; its install script downloads it, so scripts are off.
  if (must('ui core: npm ci', 'npm', ['ci', '--prefer-offline', '--ignore-scripts'], uiDir).ok) {
    must('ui core: SDK build', 'npm', ['run', 'build'], path.join(uiDir, 'sdk'));
    must('ui core: renderer build', 'node', ['--input-type=module', '-e', "const m = await import('./scripts/build-renderer.mjs'); await m.buildRenderer({ quiet: true });"], uiDir);
    must('ui core: type-check (tsc -b)', path.join(uiDir, 'node_modules', '.bin', 'tsc'), ['-b'], uiDir);
    if (!fs.existsSync(path.join(uiDir, 'renderer', 'dist', 'index.html'))) fail('ui core: no renderer build');
  }
  return finish(started, packedPlugins, plugins);
}

function finish(started, packedPlugins = [], plugins = []) {
  fs.mkdirSync(RESULTS, { recursive: true });
  fs.writeFileSync(
    path.join(RESULTS, 'split-check.json'),
    JSON.stringify({ steps: rows, packed: packedPlugins, plugins, failures: failures.map((f) => f.split('\n')[0]), total_ms: Date.now() - started, finished: new Date().toISOString() }, null, 2) + '\n',
  );
  log('results saved to packer/results/split-check.json');
  if (failures.length) {
    console.error(`\n${failures.length} failure(s):\n\n${failures.join('\n\n')}`);
    process.exitCode = 1;
    return;
  }
  log(`${rows.length} steps passed; ${packedPlugins.length} standalone plugin folders packed`);
  console.log('split check passed');
}

try {
  main();
} catch (error) {
  console.error(`\nFAILED: ${error.stack || error.message}`);
  process.exitCode = 1;
}
