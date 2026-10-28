#!/usr/bin/env node
// What each released part is built from, and its digest (12 hex characters). The release scripts
// compare the digests with the catalog's to know what changed; scripts/build-app.sh names packages
// by them. A part counts only what it really builds with:
//
//   plugin <dir>  the plugin's folder; for its Rust crates (every Cargo.toml in the folder) the
//                 local crates they depend on by path (lang-kit, model-kit), the frame's interface
//                 in protocol/ and the protocol contracts they name as features of cc-protocol
//                 (with the features those need), and rust-toolchain.toml; for its UI packages the
//                 SDK's build preset (ui/sdk/vite.mjs), or the whole SDK when it ships the SDK's
//                 runtime (the window, window@1)
//   frame         frame/ with its protocol files, the Electron shell (ui/shell), what the frame
//                 release takes from ui/package.json (name, main, type, dependencies, Electron's
//                 version), scripts/app/cortex.frame.toml and electron.sh, rust-toolchain.toml
//   interface     the frame's interface: protocol/ without the contracts and helpers that are
//                 features (a plugin needs a frame at least as new as the one that brought it)
//   npm           the npx launcher (npm/cortexcode)
//
//   digest.mjs <part> [dir]          the digest
//   digest.mjs paths <part> [dir]    the paths it covers, one per line (for uncommitted changes)

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const REPO = path.resolve(import.meta.dirname, '..', '..');
const PROTOCOL = 'protocol';

/** The files git tracks or would track under `paths` that exist, sorted (repository-relative). */
function sources(paths) {
  if (paths.length === 0) return [];
  const out = execFileSync('git', ['ls-files', '-co', '--exclude-standard', '-z', '--', ...paths], { cwd: REPO, maxBuffer: 256 * 1024 * 1024 });
  return [...new Set(out.toString().split('\0').filter((file) => file && fs.statSync(path.join(REPO, file), { throwIfNoEntry: false })?.isFile()))].sort();
}

const read = (file) => fs.readFileSync(path.join(REPO, file), 'utf8');

/** `[features]` of protocol/Cargo.toml: feature -> the features it needs. */
function protocolFeatures() {
  const section = /^\[features\]\n([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(read(`${PROTOCOL}/Cargo.toml`))?.[1] ?? '';
  const features = new Map();
  for (const match of section.matchAll(/^([a-z_]+)\s*=\s*\[([^\]]*)\]/gm)) {
    if (match[1] === 'preserve_order') continue;
    features.set(match[1], [...match[2].matchAll(/"([a-z_]+)"/g)].map((m) => m[1]));
  }
  return features;
}

/** The files of one protocol feature: contracts/<f>.rs and contracts/<f>/, or src/<f>.rs. */
function featureFiles(feature, files) {
  return files.filter((file) => file === `${PROTOCOL}/src/contracts/${feature}.rs` || file.startsWith(`${PROTOCOL}/src/contracts/${feature}/`) || file === `${PROTOCOL}/src/${feature}.rs`);
}

/** The frame's interface: every protocol file that belongs to no feature. */
function interfaceFiles() {
  const files = sources([PROTOCOL]);
  const features = new Set([...protocolFeatures().keys()].flatMap((feature) => featureFiles(feature, files)));
  return files.filter((file) => !features.has(file));
}

/**
 * What Rust crates build with, from their Cargo.toml files: the local crates they reach by path
 * (other than protocol, followed to theirs), and the protocol features they name (closed over the
 * features those need).
 */
function rustInputs(manifests) {
  const local = new Set();
  const named = new Set();
  const seen = new Set();
  const visit = (manifest) => {
    if (seen.has(manifest)) return;
    seen.add(manifest);
    const text = read(manifest);
    const dir = path.dirname(manifest);
    for (const line of text.split('\n')) {
      const dependency = /^\s*([A-Za-z0-9_-]+)\s*=\s*\{(.*)\}\s*$/.exec(line);
      const where = dependency && /\bpath\s*=\s*"([^"]+)"/.exec(dependency[2]);
      if (!where) continue;
      const target = path.relative(REPO, path.resolve(REPO, dir, where[1]));
      if (target === PROTOCOL) {
        for (const feature of /\bfeatures\s*=\s*\[([^\]]*)\]/.exec(dependency[2])?.[1].matchAll(/"([a-z_]+)"/g) ?? []) named.add(feature[1]);
      } else if (!target.startsWith('..') && fs.existsSync(path.join(REPO, target, 'Cargo.toml'))) {
        local.add(target);
        visit(path.join(target, 'Cargo.toml'));
      }
    }
  };
  manifests.forEach(visit);
  const graph = protocolFeatures();
  const features = new Set();
  const close = (feature) => {
    if (features.has(feature) || !graph.has(feature)) return;
    features.add(feature);
    graph.get(feature).forEach(close);
  };
  named.forEach(close);
  return { local: [...local].sort(), features: [...features].sort() };
}

function protocolFilesFor(features) {
  const files = sources([PROTOCOL]);
  return [...interfaceFiles(), ...features.flatMap((feature) => featureFiles(feature, files))];
}

/** The paths a part covers (for uncommitted changes) and its inputs: files and extra texts. */
function inputs(part, dir) {
  if (part === 'interface') {
    return { paths: [PROTOCOL], files: interfaceFiles(), texts: [] };
  }
  if (part === 'npm') {
    return { paths: ['npm/cortexcode'], files: sources(['npm/cortexcode']), texts: [] };
  }
  if (part === 'frame') {
    const own = ['frame', 'ui/shell', 'scripts/app/cortex.frame.toml', 'scripts/app/electron.sh', 'rust-toolchain.toml'];
    const rust = rustInputs(['frame/Cargo.toml']);
    const ui = JSON.parse(read('ui/package.json'));
    const shipped = JSON.stringify({ name: ui.name, main: ui.main, type: ui.type, dependencies: ui.dependencies, electron: ui.devDependencies?.electron });
    return {
      paths: [...own, ...rust.local, PROTOCOL, 'ui/package.json'],
      files: [...sources([...own, ...rust.local]), ...protocolFilesFor(rust.features)],
      texts: [['ui/package.json (shipped part)', shipped]],
    };
  }
  if (part === 'plugin') {
    if (!dir || !fs.existsSync(path.join(REPO, dir, 'plugin.toml'))) throw new Error(`no plugin in ${dir}`);
    const own = sources([dir]);
    const manifests = own.filter((file) => path.basename(file) === 'Cargo.toml');
    const paths = [dir];
    const files = [...own];
    if (manifests.length > 0) {
      const rust = rustInputs(manifests);
      paths.push(...rust.local, PROTOCOL, 'rust-toolchain.toml');
      files.push(...sources([...rust.local, 'rust-toolchain.toml']), ...protocolFilesFor(rust.features));
    }
    if (own.some((file) => path.basename(file) === 'package.json' && file.startsWith(`${dir}/ui`))) {
      // The window ships the SDK's runtime; other UI packages only build with its preset.
      const window = /^\s*slot\s*=\s*"window@1"/m.test(read(path.join(dir, 'plugin.toml')));
      const sdk = window ? ['ui/sdk'] : ['ui/sdk/vite.mjs'];
      paths.push(...sdk);
      files.push(...sources(sdk));
    }
    return { paths, files, texts: [] };
  }
  throw new Error(`unknown part ${part} (plugin <dir> | frame | interface | npm)`);
}

function digest({ files, texts }) {
  const lines = [...new Set(files)].sort().map((file) => `${createHash('sha256').update(fs.readFileSync(path.join(REPO, file))).digest('hex')}  ${file}`);
  for (const [name, text] of texts) lines.push(`${createHash('sha256').update(text).digest('hex')}  ${name}`);
  return createHash('sha256').update(lines.join('\n') + '\n').digest('hex').slice(0, 12);
}

const [command, ...rest] = process.argv.slice(2);
try {
  if (command === 'paths') {
    process.stdout.write([...new Set(inputs(rest[0], rest[1]).paths)].join('\n') + '\n');
  } else {
    process.stdout.write(digest(inputs(command, rest[0])) + '\n');
  }
} catch (error) {
  process.stderr.write(`digest: ${error.message}\n`);
  process.exit(1);
}
