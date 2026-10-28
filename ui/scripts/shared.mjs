// Paths the UI scripts share.
import { fileURLToPath } from 'node:url';
import path from 'node:path';

export const UI_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const PLUGINS_DIST = path.join(UI_DIR, 'plugins-dist');
