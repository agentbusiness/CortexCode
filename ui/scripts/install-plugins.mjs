// Installs built plugin packages (their dist/ with manifest.json) into the directory the frame
// serves UI plugins from (default ui/plugins-dist/<id>/). A watcher never sees half a build:
// chunks and assets first, then the stylesheet, the entry module, the manifest; files of the
// previous build that this one does not have are removed last.
//
//   node scripts/install-plugins.mjs [--only <id>,<id>] [--out <dir>]

import { randomBytes } from 'node:crypto';
import fs from 'node:fs/promises';
import path from 'node:path';
import { PLUGINS_DIST } from './shared.mjs';
import { uiPackages } from './packages.mjs';

async function walk(dir) {
  const out = [];
  for (const entry of await fs.readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...await walk(full));
    else out.push(full);
  }
  return out;
}

async function removeEmptyDirs(dir) {
  for (const entry of await fs.readdir(dir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const full = path.join(dir, entry.name);
    await removeEmptyDirs(full);
    if ((await fs.readdir(full)).length === 0) await fs.rmdir(full);
  }
}

/** Copy one built package's dist/ into `<outRoot>/<id>/`. Returns its manifest. */
export async function installDist(distDir, outRoot = PLUGINS_DIST) {
  const manifest = JSON.parse(await fs.readFile(path.join(distDir, 'manifest.json'), 'utf8'));
  const target = path.join(outRoot, manifest.id);
  // Each install stages in its own directory, so installs into one root can run side by side.
  const temp = path.join(outRoot, `.staging-${manifest.id}-${randomBytes(4).toString('hex')}`);
  await fs.mkdir(target, { recursive: true });
  await fs.mkdir(temp, { recursive: true });
  const files = (await walk(distDir)).map((file) => path.relative(distDir, file)).filter((file) => file !== 'manifest.json');
  const order = (name) => (name === manifest.entry ? 2 : name === manifest.css ? 1 : 0);
  files.sort((a, b) => order(a) - order(b));
  for (const name of files) {
    const destination = path.join(target, name);
    await fs.mkdir(path.dirname(destination), { recursive: true });
    const staged = path.join(temp, `${path.basename(name)}.${randomBytes(3).toString('hex')}.tmp`);
    await fs.copyFile(path.join(distDir, name), staged);
    await fs.rename(staged, destination);
  }
  const staged = path.join(temp, `manifest.${randomBytes(3).toString('hex')}.tmp`);
  await fs.copyFile(path.join(distDir, 'manifest.json'), staged);
  await fs.rename(staged, path.join(target, 'manifest.json'));
  const keep = new Set([...files, 'manifest.json']);
  for (const existing of (await walk(target)).map((file) => path.relative(target, file))) {
    if (!keep.has(existing)) await fs.rm(path.join(target, existing), { force: true });
  }
  await removeEmptyDirs(target);
  await fs.rm(temp, { recursive: true, force: true });
  return manifest;
}

export async function installAll({ only, outRoot = PLUGINS_DIST } = {}) {
  const out = [];
  const known = new Set();
  for (const pkg of uiPackages()) {
    if (only && !only.includes(pkg.id)) continue;
    known.add(pkg.id);
    out.push(await installDist(path.join(pkg.dir, 'dist'), outRoot));
  }
  if (!only) {
    // Plugins no package provides any more leave the served directory.
    for (const entry of await fs.readdir(outRoot, { withFileTypes: true }).catch(() => [])) {
      if (entry.isDirectory() && !entry.name.startsWith('.') && !known.has(entry.name)) await fs.rm(path.join(outRoot, entry.name), { recursive: true, force: true });
    }
  }
  return out;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const args = process.argv.slice(2);
  const onlyAt = args.indexOf('--only');
  const outAt = args.indexOf('--out');
  installAll({ only: onlyAt >= 0 ? args[onlyAt + 1].split(',') : undefined, outRoot: outAt >= 0 ? path.resolve(args[outAt + 1]) : PLUGINS_DIST })
    .then((list) => { for (const m of list) console.log(`installed ${m.id} ${m.version} (${m.slot})`); })
    .catch((error) => { console.error(error); process.exit(1); });
}
