// The UI's packages: the SDK, and every UI plugin package (a package.json with a `cortex.slot`).
// Plugins are self-contained folders in the repository's plugins/<name>/ (plugin.toml, backend/,
// ui/, …): their UI packages live in plugins/<name>/ui/<package>/ (one npm workspace per plugin,
// rooted at plugins/<name>/ui) or plugins/<name>/ui/ itself when the plugin has a single package.
// Packages not yet moved into a plugin folder stay in ui/plugins/<plugin>/<package>/ (workspaces of
// the ui/ root). Shell modules (`cortex.shellModule`) are listed apart.
import fs from 'node:fs';
import path from 'node:path';
import { UI_DIR } from './shared.mjs';

export const SDK_DIR = path.join(UI_DIR, 'sdk');
/** The repository's plugin folders. */
export const REPO_PLUGINS_DIR = path.resolve(UI_DIR, '..', 'plugins');

function readPackage(dir) {
  try {
    return JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
  } catch {
    return null;
  }
}

function subdirs(dir) {
  try {
    return fs.readdirSync(dir, { withFileTypes: true }).filter((e) => e.isDirectory() && e.name !== 'node_modules' && !e.name.startsWith('.')).map((e) => e.name).sort();
  } catch {
    return [];
  }
}

function describe(dir, npmRoot, plugin) {
  const pkg = readPackage(dir);
  if (!pkg?.cortex?.slot) return null;
  return { dir, name: pkg.name, id: pkg.cortex.id, slot: pkg.cortex.slot, version: pkg.version, npmRoot, plugin };
}

/** [{ dir, name, id, slot, version, npmRoot, plugin }] for every UI plugin package. */
export function uiPackages() {
  const out = [];
  // Plugin folders: plugins/<name>/ui (a package itself, or an npm workspace of packages).
  for (const plugin of subdirs(REPO_PLUGINS_DIR)) {
    const ui = path.join(REPO_PLUGINS_DIR, plugin, 'ui');
    const single = describe(ui, ui, plugin);
    if (single) {
      out.push(single);
      continue;
    }
    for (const name of subdirs(ui)) {
      const found = describe(path.join(ui, name), ui, plugin);
      if (found) out.push(found);
    }
  }
  // Packages still inside ui/plugins/<plugin>/<package> (workspaces of ui/).
  const legacy = path.join(UI_DIR, 'plugins');
  for (const plugin of subdirs(legacy)) {
    for (const name of subdirs(path.join(legacy, plugin))) {
      const found = describe(path.join(legacy, plugin, name), UI_DIR, plugin);
      if (found) out.push(found);
    }
  }
  return out;
}

/** The npm roots (directories with their own package-lock.json) the UI packages install from. */
export function npmRoots() {
  return [...new Set(uiPackages().map((pkg) => pkg.npmRoot))].filter((root) => root !== UI_DIR);
}

/** [{ dir, name, id, plugin }] for every shell module package (plugins/<name>/shell). */
export function shellModules() {
  const out = [];
  const roots = [REPO_PLUGINS_DIR, path.join(UI_DIR, 'plugins')];
  for (const root of roots) {
    for (const plugin of subdirs(root)) {
      for (const name of ['shell', 'shell-module']) {
        const dir = path.join(root, plugin, name);
        const pkg = readPackage(dir);
        if (pkg?.cortex?.shellModule) out.push({ dir, name: pkg.name, id: pkg.cortex.id, plugin });
      }
    }
  }
  return out;
}
