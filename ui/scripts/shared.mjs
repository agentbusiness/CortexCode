// The modules every UI plugin shares with the workbench through the import map, so the whole
// renderer runs one React and one @cortex/sdk instance. Paths are relative to the renderer's dist.
import { fileURLToPath } from 'node:url';
import path from 'node:path';

export const UI_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const RENDERER_DIR = path.join(UI_DIR, 'renderer');
export const RENDERER_DIST = path.join(RENDERER_DIR, 'dist');
export const PLUGINS_DIR = path.join(UI_DIR, 'plugins');
export const PLUGINS_DIST = path.join(UI_DIR, 'plugins-dist');
export const SDK_ENTRY = path.join(UI_DIR, 'sdk', 'src', 'index.ts');

/** Bare specifier -> file under `<renderer dist>/vendor/`. */
export const SHARED_MODULES = {
  react: 'react.js',
  'react/jsx-runtime': 'react-jsx-runtime.js',
  'react-dom': 'react-dom.js',
  'react-dom/client': 'react-dom-client.js',
  '@cortex/sdk': 'cortex-sdk.js',
};

/** The URL base the frame serves the renderer under. */
export const UI_BASE = '/ui/';

export function importMap() {
  const imports = {};
  for (const [specifier, file] of Object.entries(SHARED_MODULES)) imports[specifier] = `${UI_BASE}vendor/${file}`;
  return { imports };
}

export const EXTERNALS = Object.keys(SHARED_MODULES);
