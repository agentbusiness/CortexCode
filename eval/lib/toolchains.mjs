// Toolchains and dependency caches. `ensure*` functions run at prepare time (network allowed);
// everything they produce lives under eval/cache/toolchains so runs and scoring work offline.
import fs from 'node:fs';
import path from 'node:path';
import { REPO_ROOT, RUST_TOOLCHAIN, SRC, TOOLCHAINS } from './paths.mjs';
import { ensureDir, exists, log, runOk, writeJson, writeText, cloneTree } from './util.mjs';

export const PINS = {
  python: '3.12',
  pyTools: ['tree-sitter==0.26.0', 'tree-sitter-language-pack==1.22.1', 'ruff==0.17.0'],
  polyglotPy: ['pytest==9.1.1'],
  aider: ['aider-chat==0.86.2'],
  mini: ['mini-swe-agent==2.4.6'],
  prettier: '3.9.10',
  pnpm: '11.3.0',
  // Exactly the package set aider's benchmark Dockerfile installs into /npm-install (jest pinned to
  // the version the exercises' package.json requires, since the Dockerfile leaves it unpinned).
  polyglotJs: {
    jest: '29.7.0',
    '@babel/core': '7.25.2',
    '@exercism/babel-preset-javascript': '0.2.1',
    '@exercism/eslint-config-javascript': '0.6.0',
    '@types/jest': '29.5.12',
    '@types/node': '20.12.12',
    'babel-jest': '29.6.4',
    'core-js': '3.37.1',
    eslint: '8.49.0',
  },
  batsCore: '0c92eb4c418d59fcb1a1a8c1d7a015f8149689f2',
  // Container suites: the official harnesses, each in its own uv venv from a hashed lock file
  // (suites/swebench/requirements.lock, suites/terminalbench/requirements.lock).
  swebench: 'swebench==4.1.0',
  harbor: 'harbor==0.24.0',
};

export const T = {
  pyTools: path.join(TOOLCHAINS, 'py-tools'),
  polyglotPy: path.join(TOOLCHAINS, 'polyglot-py'),
  polyglotJs: path.join(TOOLCHAINS, 'polyglot-js'),
  nodeTools: path.join(TOOLCHAINS, 'node-tools'),
  aider: path.join(TOOLCHAINS, 'aider'),
  mini: path.join(TOOLCHAINS, 'mini'),
  cargoHome: path.join(TOOLCHAINS, 'cargo-home'),
  bats: path.join(TOOLCHAINS, 'bats-core'),
  venvs: path.join(TOOLCHAINS, 'venvs'),
  nodeModules: path.join(TOOLCHAINS, 'node_modules_cache'),
  rustTargets: path.join(TOOLCHAINS, 'rust-targets'),
  uvCache: path.join(TOOLCHAINS, 'uv-cache'),
  pnpmStore: path.join(TOOLCHAINS, 'pnpm-store'),
  // Language-specific suites (prepared by eval/suites/*/prepare.mjs).
  uvPython: path.join(TOOLCHAINS, 'uv-python'),
  lcb: path.join(TOOLCHAINS, 'lcb'),
  bcb: path.join(TOOLCHAINS, 'bcb'),
  rustup: path.join(TOOLCHAINS, 'rustup'),
  go: path.join(TOOLCHAINS, 'go'),
  goAgentCache: path.join(TOOLCHAINS, 'go-cache-agent'),
  jdk: path.join(TOOLCHAINS, 'jdk-21'),
  gradleHome: path.join(TOOLCHAINS, 'gradle-home'),
  gradleAgentHome: path.join(TOOLCHAINS, 'gradle-home-agent'),
  boost: path.join(TOOLCHAINS, 'boost'),
  webbench: path.join(TOOLCHAINS, 'web-bench'),
  playwright: path.join(TOOLCHAINS, 'ms-playwright'),
  swebench: path.join(TOOLCHAINS, 'swebench'),
  harbor: path.join(TOOLCHAINS, 'harbor'),
};

/** Toolchains of the language-specific suites that agents and scoring may read but never modify. */
export const LANG_READ_ONLY = [T.uvPython, T.lcb, T.bcb, T.rustup, T.go, T.jdk, T.gradleHome, T.boost, T.webbench, T.playwright];

export const py = (venv) => path.join(venv, 'bin', 'python');

async function ensureVenv(dir, packages, stampName = '.cceval-stamp.json') {
  const stamp = path.join(dir, stampName);
  const want = JSON.stringify(packages);
  if (await exists(stamp) && fs.readFileSync(stamp, 'utf8') === want) return dir;
  log(`creating venv ${path.relative(TOOLCHAINS, dir)}: ${packages.join(' ')}`);
  await runOk('uv', ['venv', '-q', '--clear', '--python', PINS.python, dir], { timeoutMs: 600000 });
  await runOk('uv', ['pip', 'install', '-q', '--python', py(dir), ...packages], { timeoutMs: 1800000 });
  await writeText(stamp, want);
  return dir;
}

export const ensurePyTools = () => ensureVenv(T.pyTools, PINS.pyTools);
export const ensurePolyglotPy = () => ensureVenv(T.polyglotPy, PINS.polyglotPy);
export const ensureAider = () => ensureVenv(T.aider, PINS.aider);
export const ensureMini = () => ensureVenv(T.mini, PINS.mini);

async function ensureNpmDir(dir, deps) {
  const stamp = path.join(dir, '.cceval-stamp.json');
  const want = JSON.stringify(deps);
  if (await exists(stamp) && fs.readFileSync(stamp, 'utf8') === want && await exists(path.join(dir, 'node_modules'))) return dir;
  await ensureDir(dir);
  await writeJson(path.join(dir, 'package.json'), { name: path.basename(dir), private: true, version: '0.0.0', dependencies: deps });
  log(`npm install in ${path.relative(TOOLCHAINS, dir)}`);
  await runOk('npm', ['install', '--no-audit', '--no-fund', '--loglevel=error'], { cwd: dir, timeoutMs: 1800000 });
  await writeText(stamp, want);
  return dir;
}

export const ensureNodeTools = () => ensureNpmDir(T.nodeTools, { prettier: PINS.prettier, pnpm: PINS.pnpm });
export const ensurePolyglotJs = () => ensureNpmDir(T.polyglotJs, PINS.polyglotJs);

export async function ensureBats() {
  const marker = path.join(T.bats, 'bin', 'bats');
  if (await exists(marker)) return T.bats;
  const src = path.join(SRC, 'rz-bats-core');
  if (!await exists(src)) {
    await runOk('git', ['clone', '-q', 'https://github.com/bats-core/bats-core.git', src], { timeoutMs: 600000 });
  }
  await cloneTree(src, T.bats);
  await runOk('git', ['-C', T.bats, 'checkout', '-q', PINS.batsCore]);
  await runOk('rm', ['-rf', path.join(T.bats, '.git')]);
  return T.bats;
}

export function prettierBin() {
  return path.join(T.nodeTools, 'node_modules', '.bin', 'prettier');
}

export function pnpmBin() {
  return path.join(T.nodeTools, 'node_modules', '.bin', 'pnpm');
}

export function ruffBin() {
  return path.join(T.pyTools, 'bin', 'ruff');
}

/**
 * Environment for agents and scoring: offline package managers, pinned Rust toolchain, cached
 * cargo registry, and the suite's interpreters on PATH. Network denial itself is the sandbox's job;
 * these settings make tools fail fast instead of retrying downloads.
 */
export function offlineEnv(extraPath = []) {
  const basePath = (process.env.PATH ?? '/usr/bin:/bin:/usr/sbin:/sbin').split(':');
  const sdk = repoSdkRoot();
  return {
    ...(sdk ? { SDKROOT: sdk } : {}),
    PATH: [...extraPath, ...basePath].join(':'),
    CARGO_HOME: T.cargoHome,
    CARGO_NET_OFFLINE: 'true',
    RUSTUP_TOOLCHAIN: RUST_TOOLCHAIN,
    // cargo-home holds only the registry cache; rustup binaries come from the user's install.
    RUSTUP_HOME: process.env.RUSTUP_HOME ?? path.join(process.env.HOME ?? '', '.rustup'),
    npm_config_offline: 'true',
    UV_OFFLINE: '1',
    UV_CACHE_DIR: T.uvCache,
    PIP_NO_INDEX: '1',
    PYTHONDONTWRITEBYTECODE: '1',
    HOMEBREW_NO_AUTO_UPDATE: '1',
    CI: '1',
    // Fixed terminal type: some suites (bats-core) call tput; results must not depend on the caller.
    TERM: 'dumb',
  };
}

/**
 * The SDKROOT the repository pins in .cargo/config.toml ([env] SDKROOT). Workspaces live outside
 * the repository, so cargo would not see that file; the same value is passed explicitly.
 */
export function repoSdkRoot() {
  try {
    const text = fs.readFileSync(path.join(REPO_ROOT, '.cargo', 'config.toml'), 'utf8');
    const m = text.match(/^\s*SDKROOT\s*=\s*"([^"]+)"/m);
    return m ? m[1] : process.env.SDKROOT ?? null;
  } catch {
    return process.env.SDKROOT ?? null;
  }
}

/** cargo with the pinned toolchain even outside the repo (rustup proxy honours RUSTUP_TOOLCHAIN). */
export function cargoCmd() {
  const home = process.env.HOME ?? '';
  const proxy = path.join(home, '.cargo', 'bin', 'cargo');
  return fs.existsSync(proxy) ? proxy : 'cargo';
}

/**
 * A uv venv with a uv-managed CPython (installed under T.uvPython, never system Python or
 * Homebrew). `install` is a list of argument lists for `uv pip install` (e.g. [['-r', lockfile],
 * ['--no-deps', '/path/to/pkg']]). Re-created when the stamp (python + install args) changes.
 */
export async function ensureManagedVenv(dir, pythonVersion, install, { env: extraEnv = {}, stampExtra = null } = {}) {
  const stamp = path.join(dir, '.cceval-stamp.json');
  const want = JSON.stringify({ python: pythonVersion, install, ...(stampExtra ? { extra: stampExtra } : {}) });
  if (await exists(stamp) && fs.readFileSync(stamp, 'utf8') === want) return dir;
  const env = { ...process.env, UV_PYTHON_INSTALL_DIR: T.uvPython, UV_CACHE_DIR: T.uvCache, UV_PYTHON_PREFERENCE: 'only-managed', ...extraEnv };
  log(`creating venv ${path.relative(TOOLCHAINS, dir)} (CPython ${pythonVersion})`);
  await runOk('uv', ['python', 'install', pythonVersion], { env, timeoutMs: 1800000 });
  await runOk('uv', ['venv', '-q', '--clear', '--python', pythonVersion, dir], { env, timeoutMs: 600000 });
  for (const args of install) {
    await runOk('uv', ['pip', 'install', '-q', '--python', py(dir), ...args], { env, timeoutMs: 3600000 });
  }
  await writeText(stamp, want);
  return dir;
}
