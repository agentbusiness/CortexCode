// Fixed locations used by every part of the eval harness.
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';

export const EVAL_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const REPO_ROOT = path.resolve(EVAL_ROOT, '..');
export const CACHE = path.join(EVAL_ROOT, 'cache');
/** Raw upstream clones (git history, solutions). Never readable by an agent under evaluation. */
export const SRC = path.join(CACHE, 'src');
/** Materialized tasks: hidden tests, originals, manifests. Never readable by an agent. */
export const HIDDEN = path.join(CACHE, 'hidden');
/** Toolchains and dependency caches (venvs, node_modules, cargo home). Readable by agents. */
export const TOOLCHAINS = path.join(CACHE, 'toolchains');
export const RESULTS = path.join(EVAL_ROOT, 'results');
export const MANIFEST = path.join(EVAL_ROOT, 'manifest.json');

export const FRAME_BIN = path.join(REPO_ROOT, 'frame', 'target', 'release', 'cortex-frame');
export const DEV_CONFIG = path.join(REPO_ROOT, 'cortex.dev.toml');

/** An optional local repository to mine tasks from (REFERENCE_REPO_DIR): read-only source, copied into the cache by `git clone`. */
export const REFERENCE_REPO = process.env.REFERENCE_REPO_DIR || null;
/** Local source folders the model must never read (the reference repository, when there is one). */
export const LOCAL_REFS = REFERENCE_REPO ? [REFERENCE_REPO] : [];

export const RUST_TOOLCHAIN = '1.94.1';

export function tmpRoot() {
  return process.env.TMPDIR ? path.resolve(process.env.TMPDIR) : os.tmpdir();
}
