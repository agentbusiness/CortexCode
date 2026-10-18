// Aider Polyglot task protocol: workspace, hidden tests, aider's test commands and prompts.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { LOCAL_REFS, HIDDEN, SRC } from '../../lib/paths.mjs';
import { scoreInCopy } from '../../lib/score.mjs';
import { cargoCmd, offlineEnv, T } from '../../lib/toolchains.mjs';
import { cloneTree, readJson } from '../../lib/util.mjs';
import { GO_BIN, JAVA_HOME } from './langs.mjs';

export const POLYGLOT = {
  url: 'https://github.com/Aider-AI/polyglot-benchmark.git',
  commit: '7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f',
  aiderUrl: 'https://github.com/Aider-AI/aider.git',
  aiderCommit: '5dc9490bb35f9729ef2c95d00a19ccd30c26339c',
  // Every language in the pinned checkout. Python, JavaScript and Rust were the first three; C++, Go
  // and Java use the toolchains in langs.mjs. `--lang` selects any subset.
  languages: ['python', 'javascript', 'rust', 'cpp', 'go', 'java'],
  tries: 2,
  // aider's run_unit_tests timeout (60 * 3 seconds).
  testTimeoutMs: 180000,
};

const DIR = path.join(HIDDEN, 'polyglot');

export function taskDir(id) {
  return path.join(DIR, id);
}

export async function loadPolyglotTask(id) {
  return readJson(path.join(DIR, id, 'task.json'));
}

export function hiddenFiles(task) {
  return task.test_files.map((rel) => ({ rel, abs: path.join(DIR, task.id, 'tests', rel) }));
}

/** Files that must also never reach the agent (exercism example solutions). */
export function secretFiles(task) {
  return task.example_files.map((rel) => ({ rel, abs: path.join(DIR, task.id, 'example', rel) }));
}

/**
 * Where each upstream example (reference) file goes in a workspace to form the reference solution:
 * one example and one solution file map directly (as before); otherwise the solution file with the
 * same base name (java: .meta/src/reference/java/X.java -> src/main/java/X.java), else, for
 * exercism's generic names (example.*, proof.*), the solution file with the same extension
 * (cpp: .meta/example.h -> foo.h), else next to the first solution file (java helper classes only
 * the reference uses). Structural mapping by file name.
 */
export function exampleTargets(task) {
  if (task.example_files.length === 1 && task.solution_files.length === 1) return [{ from: task.example_files[0], to: task.solution_files[0] }];
  return task.example_files.map((ex) => {
    const base = path.basename(ex);
    const to = task.solution_files.find((f) => path.basename(f) === base)
      ?? (/^(example|proof)\b/i.test(base) ? task.solution_files.find((f) => path.extname(f) === path.extname(ex)) : null)
      ?? (task.solution_files.length ? path.join(path.dirname(task.solution_files[0]), base) : null);
    return { from: ex, to };
  });
}

/** Dependency links in the agent workspace (not part of the exercise, excluded from git). */
export function workspaceLinks(task) {
  if (task.lang === 'javascript') {
    return [{ rel: 'node_modules', target: path.join(T.polyglotJs, 'node_modules'), mode: 'symlink' }];
  }
  return [];
}

export function agentPath() {
  return [path.join(T.polyglotPy, 'bin'), GO_BIN, path.join(JAVA_HOME, 'bin')];
}

/** Agent-side settings for the C++/Go/Java toolchains (their own writable caches, never scoring's). */
export function agentEnvExtra() {
  return {
    JAVA_HOME, GRADLE_USER_HOME: T.gradleAgentHome, JAVA_TOOL_OPTIONS: '-Djava.net.preferIPv4Stack=true', GOCACHE: T.goAgentCache, GOTOOLCHAIN: 'local', GOPROXY: 'off',
    GOFLAGS: '-mod=mod', GOTELEMETRY: 'off', CMAKE_PREFIX_PATH: T.boost, BOOST_ROOT: T.boost,
  };
}

// Verbatim from aider benchmark/cpp-test.sh at the pinned aider commit.
export const CPP_TEST_SH = `#!/bin/bash

# exit when any command fails
set -e

[ ! -d "build" ] && mkdir build
cd build
cmake -DEXERCISM_RUN_ALL_TESTS=1 -G "Unix Makefiles" ..
make
`;

/** aider's cleanup_test_output: drop timing text and the test dir path before showing the model. */
export function cleanupTestOutput(output, testdir) {
  return output.replace(/\bin \d+\.\d+s\b/g, '').split(testdir).join(path.basename(testdir));
}

function testCommand(task) {
  // aider TEST_COMMANDS keyed by the test files' extension.
  if (task.lang === 'python') return [path.join(T.polyglotPy, 'bin', 'pytest')];
  if (task.lang === 'rust') return [cargoCmd(), 'test', '--', '--include-ignored'];
  if (task.lang === 'javascript') return ['npm', 'run', 'test'];
  if (task.lang === 'cpp') return ['/bin/bash', '-c', CPP_TEST_SH];
  if (task.lang === 'go') return [path.join(GO_BIN, 'go'), 'test', './...'];
  // `--offline`: dependencies come from the prepared cache (the network is denied anyway; this makes
  // Gradle fail fast instead of retrying downloads).
  if (task.lang === 'java') return ['./gradlew', 'test', '--offline'];
  throw new Error(`no test command for ${task.lang}`);
}

/** Per-language environment of one scoring run (fresh caches inside the scoring copy). */
function langEnv(task, scoreDir) {
  if (task.lang === 'go') {
    return { GOCACHE: path.join(scoreDir, '.cceval-gocache'), GOPATH: path.join(scoreDir, '.cceval-gopath'), GOTOOLCHAIN: 'local', GOPROXY: 'off', GOFLAGS: '-mod=mod', GOTELEMETRY: 'off' };
  }
  // Gradle's client talks to its (single-use) build daemon over loopback TCP; a JVM's IPv6 socket
  // reaches 127.0.0.1 as ::ffff:127.0.0.1, which the sandbox's localhost rule does not match.
  if (task.lang === 'java') return { JAVA_HOME, GRADLE_USER_HOME: path.join(scoreDir, '.cceval-gradle-home'), JAVA_TOOL_OPTIONS: '-Djava.net.preferIPv4Stack=true' };
  if (task.lang === 'cpp') return { CMAKE_PREFIX_PATH: T.boost, BOOST_ROOT: T.boost };
  return {};
}

async function setupCopy(task, dir) {
  if (task.lang === 'java') {
    // aider benchmark.py: remove @Disabled annotations from Java test files.
    for (const f of task.test_files) {
      const p = path.join(dir, f);
      const text = await fsp.readFile(p, 'utf8');
      await fsp.writeFile(p, text.replace(/@Disabled\([^)]*\)\s*\n/g, ''));
    }
    // A private clone of the pristine Gradle home (wrapper distribution + dependency cache).
    await cloneTree(T.gradleHome, path.join(dir, '.cceval-gradle-home'));
  }
  if (task.lang === 'javascript') {
    // aider npm-test.sh: link the shared node_modules and lockfile, enable skipped tests.
    await fsp.rm(path.join(dir, 'node_modules'), { force: true, recursive: true });
    await fsp.symlink(path.join(T.polyglotJs, 'node_modules'), path.join(dir, 'node_modules'));
    try { await fsp.symlink(path.join(T.polyglotJs, 'package-lock.json'), path.join(dir, 'package-lock.json')); } catch { /* exists */ }
    for (const f of task.test_files) {
      const p = path.join(dir, f);
      const text = await fsp.readFile(p, 'utf8');
      await fsp.writeFile(p, text.replace(/\bxtest\(/g, 'test('));
    }
  }
}

/** Score a workspace: fresh copy + restored tests + aider's command. */
export async function polyglotScore(task, workspace, scoreDirIn, outDir, tag) {
  // The C++ exercises' CMakeLists.txt derive the exercise (and source file) name from the directory
  // name, as in aider's per-exercise test directory; the scoring copy is therefore named by the slug.
  const scoreDir = task.lang === 'cpp' ? path.join(scoreDirIn, task.slug) : scoreDirIn;
  const env = { ...offlineEnv([path.join(T.polyglotPy, 'bin'), GO_BIN, path.join(JAVA_HOME, 'bin')]), ...langEnv(task, scoreDir) };
  if (task.lang === 'rust') env.CARGO_TARGET_DIR = path.join(scoreDir, 'target');
  const res = await scoreInCopy({
    workspace, scoreDir, outDir, hiddenFiles: hiddenFiles(task),
    // A build folder the agent's own builds left (CMake's cache names the folder it was made in, so
    // a copied one stops the scoring build): scoring builds afresh.
    deletions: task.lang === 'cpp' ? ['build'] : [],
    setup: (dir) => setupCopy(task, dir), command: testCommand(task), env,
    timeoutMs: POLYGLOT.testTimeoutMs, tag: tag.replace(/[^A-Za-z0-9_.-]/g, '_'),
    denyRead: [HIDDEN, SRC, ...LOCAL_REFS], denyWrite: [T.polyglotJs, T.polyglotPy, T.go, T.jdk, T.gradleHome, T.boost].filter((p) => fs.existsSync(p)),
  });
  res.feedback = cleanupTestOutput(res.output, scoreDir);
  return res;
}

export function followupPrompt(task, errors) {
  // aider: instructions = errors + prompts.test_failures.format(file_list=file_list). The errors
  // go as a pasted-text attachment (the way a person's long paste goes), the instructions as the
  // message: the model reads the output it needs instead of carrying the whole log in every prompt.
  return {
    // The tests stay hidden (the workspace is offline for them): said, so the model fixes from the
    // failures instead of searching the machine for the test file (seen: find / and grep -r /).
    text: `See the testing errors in the attached test output.
The tests are correct, don't try and change them. They are not in your project: work from the failures in the output.
Fix the code in ${task.file_list} to resolve the errors.
`,
    pasted: errors,
    name: 'test-output.txt',
  };
}
