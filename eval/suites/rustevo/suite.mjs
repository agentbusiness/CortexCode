// RustEvo² (Rust API evolution): each task names an API that changed between Rust releases
// (stabilized, signature change, implicit behaviour change, deprecation) and asks for a function that
// uses it. The workspace is a Cargo crate pinned (rust-toolchain.toml) to the task's Rust version;
// the agent writes src/lib.rs; RustEvo's own procedure compiles it together with the hidden test
// program with that version's `rustc --test` and runs the tests (pass@1, one attempt).
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeAdapter } from '../../lib/adapter.mjs';
import { LOCAL_REFS, HIDDEN, SRC, TOOLCHAINS } from '../../lib/paths.mjs';
import { scoreInCopy } from '../../lib/score.mjs';
import { offlineEnv, py, T } from '../../lib/toolchains.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));

export const RUSTEVO = {
  repo: 'https://github.com/SYSUSELab/RustEvo.git',
  commit: 'aed98f6f0c994a4535a40df55d6ba9d51edae817',
  tasksFile: 'Dataset/RustEvo^2.json',
  apiFile: 'Dataset/APIDocs.json',
  // The std-library tasks' to_version range (Evaluate/eval_models_rq1.py: rust_version = to_version).
  toolchains: ['1.71.0', '1.72.0', '1.73.0', '1.74.0', '1.75.0', '1.76.0', '1.77.0', '1.78.0', '1.79.0', '1.80.0', '1.81.0', '1.82.0', '1.83.0', '1.84.0'],
  host: 'aarch64-apple-darwin',
  python: '3.11',
};

export const RUSTEVO_DIR = path.join(HIDDEN, 'rustevo');
export const GRADER = path.join(HERE, 'grade.py');
export const TASK_REL = '.cceval_rustevo_task.json';
export const RUSTEVO_PY = path.join(TOOLCHAINS, 'rustevo-py');

export function rustcFor(version) {
  return path.join(T.rustup, 'toolchains', `${version}-${RUSTEVO.host}`, 'bin', 'rustc');
}

/**
 * RustEvo's code-generation prompt for standard-library tasks (get_code_generation_prompt in
 * Evaluate/eval_models_rq1.py), plus how the answer is delivered in the workspace.
 */
export function rustevoPrompt(task, api) {
  return `You are an expert Rust programmer. Write a Rust function implementation for the following task:

Task Description:
${task.query}

Required Function Signature:
\`\`\`rust
${task.function_signature}
\`\`\`

Relevant API Information:
- API Name: ${api.name ?? ''}
- API Module: ${api.module ?? ''}
- API Signature: ${api.signature ?? ''}
- API Documentation: ${api.documentation ?? ''}
- API Source Code: ${api.source_code ?? ''}
- API Changed From Version: ${api.from_version ?? ''}
- API Changed To Version: ${api.to_version ?? ''}

Requirements:
1. Implement ONLY the function with the given signature, no additional functions.
2. Your implementation MUST use the specified API: ${api.name ?? ''}
3. Make sure your code is compatible with Rust version ${api.to_version}
3. Do not include tests, main function, or any code outside the required function.
4. Do not include additional comments or explanations.

Write the function to \`src/lib.rs\` in this workspace (one file; replace the stub). The crate is pinned to Rust ${api.to_version} (rust-toolchain.toml) and is graded by compiling \`src/lib.rs\` together with hidden tests as one file with \`rustc --test\` (Rust ${api.to_version}, default edition 2015, no external crates).
`;
}

/** The workspace stub: the required signature with a body that panics. */
export function stubSource(signature) {
  return `${signature.trim().replace(/\{\s*$/, '').trim()} {\n    unimplemented!()\n}\n`;
}

export function hiddenFiles(task) {
  return [{ rel: TASK_REL, abs: path.join(RUSTEVO_DIR, task.id, 'tests', 'task-test.json') }];
}

export function rustevoEnv() {
  const env = offlineEnv([]);
  env.RUSTUP_HOME = T.rustup;
  return env;
}

export async function rustevoScore(task, workspace, scoreDir, outDir, tag, { solutionOverride } = {}) {
  return scoreInCopy({
    workspace, scoreDir, outDir, hiddenFiles: hiddenFiles(task),
    setup: solutionOverride ? (dir) => solutionOverride(dir) : undefined,
    command: [py(RUSTEVO_PY), '-I', GRADER, '--solution', 'src/lib.rs', '--task', TASK_REL, '--rustc', rustcFor(task.rust_version)],
    env: rustevoEnv(),
    timeoutMs: (30 + 30 + 120) * 1000,
    tag: tag.replace(/[^A-Za-z0-9_.-]/g, '_'),
    denyRead: [HIDDEN, SRC, ...LOCAL_REFS], denyWrite: [T.rustup, RUSTEVO_PY, T.uvPython],
  });
}

export const rustevo = makeAdapter({
  name: 'rustevo',
  title: 'RustEvo² (Rust API evolution, standard-library tasks)',
  tries: 1,
  defaultTimeoutMs: 30 * 60000,
  hiddenFiles,
  score: (task, ws, scoreDir, outDir, tag) => rustevoScore(task, ws, scoreDir, outDir, tag),
  // The workspace's rust-toolchain.toml selects the task's Rust version through the rustup proxy;
  // the harness default RUSTUP_TOOLCHAIN (1.94.1) must not override it (null = unset).
  agentEnvExtra: () => ({ RUSTUP_HOME: T.rustup, RUSTUP_TOOLCHAIN: null }),
  aiderFiles: () => ({ fnames: ['src/lib.rs'], ignore: ['Cargo.toml', 'rust-toolchain.toml'] }),
  reportNote: 'Pass = RustEvo\'s compile-and-test step (task\'s Rust version, `rustc --test`). RustEvo\'s RQ1 script additionally requires two name checks (function name present, API name present) before testing; that stricter "official" rate is reported separately from the same raw verdicts. The 208 third-party-crate tasks are excluded: RustEvo\'s evaluator compiles with bare `rustc` and no dependencies, so no solution can compile (measured on their reference solutions; RustEvo\'s published results also show 0/208 for every model).',
});
