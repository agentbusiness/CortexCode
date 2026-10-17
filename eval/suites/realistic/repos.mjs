// Repositories mined for the realistic suite. Every rule here is structural: path patterns decide
// what is source and what is test, and runner output is parsed for test counts.
import path from 'node:path';
import { REFERENCE_REPO } from '../../lib/paths.mjs';
import { T } from '../../lib/toolchains.mjs';

/** Parse executed/passed test counts from a runner's output (structural, per runner format). */
export const COUNTERS = {
  cargo(output) {
    let passed = 0, failed = 0, suites = 0;
    for (const m of output.matchAll(/test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed/g)) {
      passed += Number(m[1]); failed += Number(m[2]); suites++;
    }
    return { passed, failed, found: suites > 0 };
  },
  pytest(output) {
    const tail = output.split('\n').filter((l) => /\d+ (passed|failed|error)/.test(l)).pop() ?? '';
    const n = (re) => Number(tail.match(re)?.[1] ?? 0);
    return { passed: n(/(\d+) passed/), failed: n(/(\d+) failed/) + n(/(\d+) errors?/), found: !!tail };
  },
  vitest(output) {
    const clean = output.replace(/\x1b\[[0-9;]*m/g, '');
    const line = clean.split('\n').filter((l) => /^\s*Tests\s+/.test(l)).pop() ?? '';
    return {
      passed: Number(line.match(/(\d+) passed/)?.[1] ?? 0),
      failed: Number(line.match(/(\d+) failed/)?.[1] ?? 0),
      found: !!line,
    };
  },
  tap(output) {
    let passed = 0, failed = 0, plan = false;
    for (const l of output.split('\n')) {
      if (/^1\.\.\d+/.test(l)) plan = true;
      if (/^ok \d+/.test(l) && !/# skip/i.test(l)) passed++;
      if (/^not ok \d+/.test(l)) failed++;
    }
    return { passed, failed, found: plan };
  },
};

const isRunnableRust = (f) => /^tests\/[^/]+\.rs$/.test(f);

function justFilters(files) {
  // tests/lib.rs declares `mod <name>;` for tests/<name>.rs; test paths are `<name>::<test>`.
  const mods = new Set();
  for (const f of files) {
    const m = f.match(/^tests\/([^/]+)\.rs$/) ?? f.match(/^tests\/([^/]+)\//);
    if (!m) continue;
    if (m[1] === 'lib' || m[1] === 'test') return [];
    mods.add(`${m[1]}::`);
  }
  return [...mods].sort();
}

export const REALISTIC_REPOS = [
  {
    id: 'just', lang: 'rust', url: 'https://github.com/casey/just.git', dir: 'rz-just',
    sourceRe: /^src\/.*\.rs$/, testRe: /^tests\//, runnable: isRunnableRust,
    deps: { kind: 'cargo' }, counter: 'cargo', testTimeoutMs: 1200000,
    prebuild: ['test', '--no-run', '--test', 'integration'],
    testCommand: (files) => ['cargo', 'test', '--test', 'integration', '--', ...justFilters(files)],
  },
  // An optional local TypeScript repository (REFERENCE_REPO_DIR) with its app under web/.
  ...(REFERENCE_REPO ? [{
    id: 'reference-web', lang: 'ts', local: REFERENCE_REPO, dir: 'rz-reference-web', subdir: 'web',
    sourceRe: /^web\/src\/.*\.(ts|tsx)$/, testRe: /^web\/src\/.*\.test\.(ts|tsx)$/,
    runnable: (f) => /\.test\.(ts|tsx)$/.test(f),
    deps: { kind: 'node', manager: 'npm', lockfile: 'package-lock.json' }, counter: 'vitest', testTimeoutMs: 600000,
    testCommand: (files) => ['node_modules/.bin/vitest', 'run', '--reporter=default', ...files.map((f) => f.replace(/^web\//, ''))],
  }] : []),
  {
    id: 'zustand', lang: 'ts', url: 'https://github.com/pmndrs/zustand.git', dir: 'rz-zustand',
    sourceRe: /^src\/.*\.(ts|tsx)$/, testRe: /^tests\//, runnable: (f) => /\.test\.(ts|tsx)$/.test(f),
    deps: { kind: 'node', manager: 'pnpm', lockfile: 'pnpm-lock.yaml' }, counter: 'vitest', testTimeoutMs: 600000,
    testCommand: (files) => ['node_modules/.bin/vitest', 'run', '--reporter=default', ...files],
  },
  {
    id: 'jotai', lang: 'ts', url: 'https://github.com/pmndrs/jotai.git', dir: 'rz-jotai',
    sourceRe: /^src\/.*\.(ts|tsx)$/, testRe: /^tests\//, runnable: (f) => /\.test\.(ts|tsx)$/.test(f),
    deps: { kind: 'node', manager: 'pnpm', lockfile: 'pnpm-lock.yaml' }, counter: 'vitest', testTimeoutMs: 600000,
    testCommand: (files) => ['node_modules/.bin/vitest', 'run', '--reporter=default', ...files],
  },
  {
    id: 'click', lang: 'python', url: 'https://github.com/pallets/click.git', dir: 'rz-click',
    sourceRe: /^src\/click\/.*\.py$/, testRe: /^tests\//, runnable: (f) => /^tests\/(.*\/)?test_[^/]*\.py$/.test(f),
    deps: { kind: 'python', extras: ['pytest'] }, counter: 'pytest', testTimeoutMs: 600000,
    testCommand: (files) => ['.venv/bin/python', '-m', 'pytest', '-p', 'no:cacheprovider', '-q', ...files],
  },
  {
    id: 'more-itertools', lang: 'python', url: 'https://github.com/more-itertools/more-itertools.git', dir: 'rz-more-itertools',
    sourceRe: /^more_itertools\/.*\.pyi?$/, testRe: /^tests\//, runnable: (f) => /^tests\/test_[^/]*\.py$/.test(f),
    deps: { kind: 'python', extras: ['pytest'] }, counter: 'pytest', testTimeoutMs: 600000,
    testCommand: (files) => ['.venv/bin/python', '-m', 'pytest', '-p', 'no:cacheprovider', '-q', ...files],
  },
  {
    id: 'rbenv', lang: 'bash', url: 'https://github.com/rbenv/rbenv.git', dir: 'rz-rbenv',
    sourceRe: /^(libexec|bin|rbenv\.d|completions)\//, testRe: /^test\//, runnable: (f) => /^test\/[^/]+\.bats$/.test(f),
    deps: { kind: 'none' }, counter: 'tap', testTimeoutMs: 600000,
    testCommand: (files) => [path.join(T.bats, 'bin', 'bats'), '--tap', ...files],
  },
  {
    id: 'bats-core', lang: 'bash', url: 'https://github.com/bats-core/bats-core.git', dir: 'rz-bats-core',
    sourceRe: /^(lib|libexec|bin)\//, testRe: /^test\//, runnable: (f) => /^test\/[^/]+\.bats$/.test(f),
    deps: { kind: 'none' }, counter: 'tap', testTimeoutMs: 900000,
    testCommand: (files) => ['bin/bats', '--tap', ...files],
  },
];

/** Mining limits (structural): bounded task size so a task is a commit-sized change. */
export const MINING = {
  scanCommits: 400,
  maxSourceFiles: 6,
  maxSourceChangedLines: 400,
  // Structural floor for a usable task statement: a subject of at least 4 words or a non-empty body.
  minSubjectWords: 4,
  perRepoTarget: { just: 13, 'reference-web': 8, zustand: 6, jotai: 6, click: 8, 'more-itertools': 6, rbenv: 6, 'bats-core': 8 },
  langTarget: 11,
};
