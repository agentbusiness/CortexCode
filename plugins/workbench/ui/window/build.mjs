// Builds the window (window@1) into dist/: the page the frame serves at /ui/, its shared vendor
// modules (react, react-dom, react/jsx-runtime and @cortex/sdk in dist/vendor/, which every UI
// plugin reaches through the page's import map instead of bundling its own copy) and manifest.json.
//
// The SDK the window serves is built from its source first (the `file:` dependency), so the
// runtime every plugin runs on is the SDK of this build, never an older installed copy. The UI
// build of the whole repository (ui/scripts/build.mjs) builds the SDK itself and sets
// CORTEX_SDK_BUILT, so it is not built twice.

import { build } from 'vite';
import react from '@vitejs/plugin-react';
import { SHARED_MODULES, sourceStamp } from '@cortex/sdk/vite';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import path from 'node:path';

const DIR = import.meta.dirname;
const DIST = path.join(DIR, 'dist');
const require = createRequire(path.join(DIR, 'package.json'));
const pkg = JSON.parse(fs.readFileSync(path.join(DIR, 'package.json'), 'utf8'));

/** The URL base the frame serves the window under. */
const BASE = '/ui/';
/** Bare specifier -> file under dist/vendor/. */
const VENDOR = {
  react: 'react.js',
  'react/jsx-runtime': 'react-jsx-runtime.js',
  'react-dom': 'react-dom.js',
  'react-dom/client': 'react-dom-client.js',
  '@cortex/sdk': 'cortex-sdk.js',
};
for (const specifier of SHARED_MODULES) {
  if (!VENDOR[specifier]) throw new Error(`the SDK shares ${specifier}, which the window does not serve`);
}

function run(command, args, cwd) {
  const env = { ...process.env };
  delete env.NODE_ENV;
  const result = spawnSync(command, args, { cwd, stdio: 'inherit', env });
  if (result.status !== 0) throw new Error(`${command} ${args.join(' ')} in ${cwd} failed (${result.status})`);
}

/** Build the SDK from its source and put the build into this root's installed copy. */
function buildSdk() {
  const spec = pkg.devDependencies?.['@cortex/sdk'] ?? '';
  if (!spec.startsWith('file:') || process.env.CORTEX_SDK_BUILT) return;
  const source = path.resolve(DIR, spec.slice('file:'.length));
  run('npm', ['run', 'build', '--silent'], source);
  const installed = path.dirname(require.resolve('@cortex/sdk/package.json'));
  if (fs.realpathSync(installed) === fs.realpathSync(source)) return;
  fs.rmSync(path.join(installed, 'dist'), { recursive: true, force: true });
  fs.cpSync(path.join(source, 'dist'), path.join(installed, 'dist'), { recursive: true });
}

/** Inject the import map first in <head>, and a CSP that admits exactly that inline script. */
function importMapPlugin() {
  const imports = {};
  for (const [specifier, file] of Object.entries(VENDOR)) imports[specifier] = `${BASE}vendor/${file}`;
  const json = JSON.stringify({ imports });
  const hash = createHash('sha256').update(json).digest('base64');
  const csp = [
    "default-src 'self'",
    `script-src 'self' 'sha256-${hash}'`,
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: blob: https://avatars.githubusercontent.com",
    "font-src 'self'",
    // Another computer's frame is reached through a link the mesh opens on a local port.
    "connect-src 'self' ws://127.0.0.1:* http://127.0.0.1:*",
    "object-src 'none'",
    "base-uri 'none'",
    "frame-src 'none'",
  ].join('; ');
  return {
    name: 'cortex-import-map',
    transformIndexHtml: {
      order: 'pre',
      handler() {
        return [
          { tag: 'meta', attrs: { 'http-equiv': 'Content-Security-Policy', content: csp }, injectTo: 'head-prepend' },
          { tag: 'script', attrs: { type: 'importmap' }, children: json, injectTo: 'head-prepend' },
        ];
      },
    },
  };
}

async function buildPage() {
  await build({
    configFile: false,
    root: DIR,
    base: BASE,
    logLevel: 'warn',
    plugins: [react(), importMapPlugin()],
    build: {
      outDir: DIST,
      emptyOutDir: true,
      target: 'es2023',
      sourcemap: true,
      assetsInlineLimit: 0,
      rolldownOptions: { external: Object.keys(VENDOR) },
    },
  });
}

/**
 * One bundle holds React, ReactDOM and the JSX runtime together (so there is exactly one React),
 * and a thin facade per specifier re-exports its part. The facades' export names are read from the
 * modules themselves, so they match whatever this React version exports.
 */
async function buildVendor() {
  const vendorDir = path.join(DIST, 'vendor');
  const work = path.join(DIR, 'node_modules', '.cache', 'cortex-vendor');
  fs.mkdirSync(work, { recursive: true });
  const entry = path.join(work, 'shared-entry.js');
  fs.writeFileSync(entry, [
    "import React from 'react';",
    "import ReactDOM from 'react-dom';",
    "import ReactDOMClient from 'react-dom/client';",
    "import JSXRuntime from 'react/jsx-runtime';",
    'export { React, ReactDOM, ReactDOMClient, JSXRuntime };',
    '',
  ].join('\n'));
  await build({
    configFile: false,
    root: DIR,
    publicDir: false,
    logLevel: 'warn',
    define: { 'process.env.NODE_ENV': JSON.stringify('production') },
    build: {
      outDir: vendorDir,
      emptyOutDir: false,
      target: 'es2023',
      minify: true,
      lib: { entry, formats: ['es'], fileName: () => 'react-shared.js' },
    },
  });

  const previous = process.env.NODE_ENV;
  process.env.NODE_ENV = 'production';
  const facades = {
    react: ['React', require('react')],
    'react/jsx-runtime': ['JSXRuntime', require('react/jsx-runtime')],
    'react-dom': ['ReactDOM', require('react-dom')],
    'react-dom/client': ['ReactDOMClient', require('react-dom/client')],
  };
  if (previous === undefined) delete process.env.NODE_ENV;
  else process.env.NODE_ENV = previous;
  for (const [specifier, [binding, module]] of Object.entries(facades)) {
    const names = Object.keys(module).filter((name) => /^[A-Za-z_$][\w$]*$/.test(name) && name !== 'default');
    const source = [
      `// Facade of ${specifier} over the one shared React bundle (generated by build.mjs).`,
      `import { ${binding} as M } from './react-shared.js';`,
      'export default M;',
      `export const { ${names.join(', ')} } = M;`,
      '',
    ].join('\n');
    fs.writeFileSync(path.join(vendorDir, VENDOR[specifier]), source);
  }

  // The SDK is its own package, built on its own; the window serves that build.
  const sdkDist = path.join(path.dirname(require.resolve('@cortex/sdk/package.json')), 'dist');
  fs.copyFileSync(path.join(sdkDist, 'cortex-sdk.js'), path.join(vendorDir, VENDOR['@cortex/sdk']));
  fs.copyFileSync(path.join(sdkDist, 'cortex-sdk.js.map'), path.join(vendorDir, `${VENDOR['@cortex/sdk']}.map`));
}

function writeManifest() {
  const manifest = {
    id: pkg.cortex.id, version: pkg.version, slot: pkg.cortex.slot, entry: 'index.html', css: null,
    build: process.env.CORTEX_UI_BUILD_STAMP || sourceStamp(DIR), package: pkg.name,
  };
  fs.writeFileSync(path.join(DIST, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n');
}

buildSdk();
run(process.execPath, [require.resolve('typescript/bin/tsc'), '-p', 'tsconfig.json'], DIR);
await buildPage();
await buildVendor();
writeManifest();
