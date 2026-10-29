// The build preset every CortexCode UI plugin package uses:
//
//   // vite.config.mjs of a plugin package
//   import { pluginConfig } from '@cortex/sdk/vite';
//   export default pluginConfig(import.meta.dirname);
//
// It builds src/index.tsx as one ES module library into dist/ (index.js, style.css, chunks/),
// keeps the modules the workbench shares through its import map external (React, ReactDOM, the
// JSX runtime and @cortex/sdk), and writes dist/manifest.json — {id, version, slot, entry, css, label?, order?, flow?,
// build, sdk_imports} — from the package's `cortex` field and the names the bundle imports from the SDK. The frame serves a plugin from that directory.

import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

/** Bare specifiers the workbench provides; a plugin never bundles them. */
export const SHARED_MODULES = ['react', 'react/jsx-runtime', 'react-dom', 'react-dom/client', '@cortex/sdk'];

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function walk(dir) {
  if (!fs.existsSync(dir)) return [];
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(full));
    else out.push(full);
  }
  return out.sort();
}

/** The plugin's identity from package.json (`cortex: {id, slot}`); validated, never guessed. */
export function pluginIdentity(dir) {
  const pkg = readJson(path.join(dir, 'package.json'));
  const cortex = pkg.cortex ?? {};
  if (typeof cortex.id !== 'string' || !/^[A-Za-z0-9._@-]+$/.test(cortex.id)) throw new Error(`${pkg.name}: package.json needs cortex.id`);
  if (typeof cortex.slot !== 'string' || !/^[a-zA-Z0-9.]+@\d+$/.test(cortex.slot)) throw new Error(`${pkg.name}: package.json needs cortex.slot like "pane.code@1"`);
  if (typeof pkg.version !== 'string') throw new Error(`${pkg.name}: package.json needs a version`);
  if (cortex.label !== undefined && (typeof cortex.label !== 'string' || !cortex.label.trim())) throw new Error(`${pkg.name}: cortex.label must be a non-empty string`);
  if (cortex.order !== undefined && !Number.isFinite(cortex.order)) throw new Error(`${pkg.name}: cortex.order must be a number`);
  if (cortex.flow !== undefined && (typeof cortex.flow !== 'string' || !/^[a-z0-9-]+$/.test(cortex.flow))) throw new Error(`${pkg.name}: cortex.flow must be a flow's name like "coding"`);
  return { name: pkg.name, version: pkg.version, id: cortex.id, slot: cortex.slot, label: cortex.label, order: cortex.order, flow: cortex.flow };
}

/** Content hash of what the bundle is built from: identical sources give an identical bundle. */
export function sourceStamp(dir) {
  const hash = createHash('sha256');
  const files = [path.join(dir, 'package.json'), ...walk(path.join(dir, 'src'))];
  try {
    const require = createRequire(path.join(dir, 'package.json'));
    files.push(require.resolve('@cortex/sdk/package.json'));
  } catch {
    // Building the SDK itself.
  }
  for (const file of files) {
    hash.update(path.basename(file));
    hash.update(fs.readFileSync(file));
  }
  return hash.digest('hex').slice(0, 16);
}

/**
 * The names a bundle imports from @cortex/sdk, read off the built chunks' import and re-export
 * statements. A window whose loaded SDK lacks one of them cannot link the bundle.
 */
function sdkImports(context, bundle) {
  const names = new Set();
  for (const chunk of Object.values(bundle)) {
    if (chunk.type !== 'chunk' || !chunk.imports.includes('@cortex/sdk')) continue;
    for (const node of context.parse(chunk.code).body) {
      if (node.source?.value !== '@cortex/sdk') continue;
      if (node.type === 'ImportDeclaration') {
        for (const specifier of node.specifiers) {
          if (specifier.type === 'ImportSpecifier') names.add(specifier.imported.name ?? specifier.imported.value);
        }
      } else if (node.type === 'ExportNamedDeclaration') {
        for (const specifier of node.specifiers) names.add(specifier.local.name ?? specifier.local.value);
      }
    }
  }
  return [...names].sort();
}

function manifestPlugin(dir, identity, stamp) {
  let imports = [];
  return {
    name: 'cortex-plugin-manifest',
    apply: 'build',
    generateBundle(_options, bundle) {
      imports = sdkImports(this, bundle);
    },
    closeBundle() {
      const dist = path.join(dir, 'dist');
      const css = fs.existsSync(path.join(dist, 'style.css')) ? 'style.css' : null;
      const manifest = {
        id: identity.id, version: identity.version, slot: identity.slot, entry: 'index.js', css, build: stamp, package: identity.name,
        // How hosts present the plugin (a tab, a section, a settings panel), when it says.
        ...(identity.label !== undefined ? { label: identity.label } : {}),
        ...(identity.order !== undefined ? { order: identity.order } : {}),
        // The flow whose sessions it shows, when it belongs to one (the window picks it for them).
        ...(identity.flow !== undefined ? { flow: identity.flow } : {}),
        // What the bundle needs from the window's SDK; the workbench waits for a window that has it.
        sdk_imports: imports,
      };
      fs.writeFileSync(path.join(dist, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
    },
  };
}

export async function pluginConfig(dir) {
  const identity = pluginIdentity(dir);
  const stamp = process.env.CORTEX_UI_BUILD_STAMP || sourceStamp(dir);
  const require = createRequire(path.join(dir, 'package.json'));
  const { default: react } = await import(require.resolve('@vitejs/plugin-react'));
  const define = {
    __CORTEX_BUILD__: JSON.stringify(stamp),
    __CORTEX_PLUGIN_VERSION__: JSON.stringify(identity.version),
  };
  return {
    root: dir,
    plugins: [react(), manifestPlugin(dir, identity, stamp)],
    define: { ...define, 'process.env.NODE_ENV': JSON.stringify('production') },
    build: {
      outDir: path.join(dir, 'dist'),
      emptyOutDir: true,
      target: 'es2023',
      minify: true,
      sourcemap: true,
      assetsInlineLimit: 0,
      lib: { entry: path.join(dir, 'src/index.tsx'), formats: ['es'], fileName: () => 'index.js', cssFileName: 'style' },
      rolldownOptions: { external: SHARED_MODULES, output: { chunkFileNames: 'chunks/[name]-[hash].js' } },
    },
  };
}
