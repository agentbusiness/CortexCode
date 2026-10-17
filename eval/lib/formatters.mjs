// Canonical formatters for the edit-format benchmark's "after formatting" comparison.
// The same formatter with the same fixed options is applied to the original and to the agent's
// output; only the comparison of the two results matters.
import path from 'node:path';
import { RUST_TOOLCHAIN } from './paths.mjs';
import { prettierBin, ruffBin, T } from './toolchains.mjs';
import { run } from './util.mjs';

export function langOf(file) {
  const ext = path.extname(file);
  if (ext === '.py') return 'python';
  if (ext === '.rs') return 'rust';
  if (ext === '.tsx') return 'tsx';
  if (ext === '.ts' || ext === '.mts' || ext === '.cts') return 'typescript';
  if (ext === '.js' || ext === '.jsx' || ext === '.mjs') return 'javascript';
  return null;
}

/** Format `text` as the language of `file`. Returns {ok, text, error}. */
export async function formatSource(file, text, { rustEdition = '2021' } = {}) {
  const lang = langOf(file);
  let res;
  if (lang === 'python') {
    res = await run(ruffBin(), ['format', '--isolated', '--stdin-filename', path.basename(file), '-'], { input: text, timeoutMs: 60000 });
  } else if (lang === 'rust') {
    res = await run('rustfmt', ['--edition', rustEdition, '--emit', 'stdout', '--quiet'], {
      input: text, timeoutMs: 60000, env: { ...process.env, RUSTUP_TOOLCHAIN: RUST_TOOLCHAIN },
    });
  } else if (lang) {
    res = await run(prettierBin(), ['--no-config', '--no-editorconfig', '--stdin-filepath', path.basename(file)], {
      input: text, timeoutMs: 60000, cwd: T.nodeTools,
    });
  } else {
    return { ok: false, text: null, error: `no formatter for ${file}` };
  }
  if (res.code !== 0) return { ok: false, text: null, error: res.stderr.slice(0, 2000) };
  return { ok: true, text: res.stdout, error: null };
}
