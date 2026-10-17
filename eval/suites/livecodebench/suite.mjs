// LiveCodeBench (code generation, release_v6 window) as an agentic task: the problem statement is
// the prompt, the agent writes `solution.py`, and LiveCodeBench's own grader runs every public and
// private test (pass@1, one attempt). See prepare.mjs for the data pin and the contamination note.
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeAdapter } from '../../lib/adapter.mjs';
import { LOCAL_REFS, HIDDEN, SRC } from '../../lib/paths.mjs';
import { scoreInCopy } from '../../lib/score.mjs';
import { offlineEnv, py, T } from '../../lib/toolchains.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));

export const LCB = {
  // Hugging Face dataset livecodebench/code_generation_lite at its latest revision; release_v6 is the
  // union of test.jsonl .. test6.jsonl. Only the two newest increments are downloaded (see prepare.mjs).
  dataset: 'livecodebench/code_generation_lite',
  datasetRevision: '0fe84c3912ea0c4d4a78037083943e8f0c4dd505',
  files: [
    { name: 'test5.jsonl', sha256: '7f77571c2a6df0c2a72a3277650309f67e01e0008e18117e624633df53f81214', release: 'release_v5 increment' },
    { name: 'test6.jsonl', sha256: 'bb4c364f71921c4495a6ad15abe1a927350b720009f4933e2e71f8af0f6fd1f5', release: 'release_v6 increment' },
  ],
  // Grader source: LiveCodeBench GitHub at a pinned commit.
  repo: 'https://github.com/LiveCodeBench/LiveCodeBench.git',
  commit: '28fef95ea8c9f7a547c8329f2cd3d32b92c1fa24',
  graderFiles: ['lcb_runner/evaluation/compute_code_generation_metrics.py', 'lcb_runner/evaluation/testing_util.py', 'lcb_runner/evaluation/pass_k_utils.py'],
  python: '3.11',
  venv: ['--compile-bytecode', 'numpy==2.2.6', 'tqdm==4.67.1'],
  timeoutPerTest: 6,
};

export const LCB_DIR = path.join(HIDDEN, 'livecodebench');
export const GRADER = path.join(HERE, 'grade.py');
export const TESTS_REL = '.cceval_lcb_tests.json';

// Verbatim from lcb_runner/prompts/code_generation.py (PromptConstants, generic template).
export const SYSTEM_MESSAGE_GENERIC = 'You are an expert Python programmer. You will be given a question (problem specification) and will generate a correct Python program that matches the specification and passes all tests.';
export const FORMATTING_MESSAGE_WITH_STARTER_CODE = 'You will use the following starter code to write the solution to the problem and enclose your code within delimiters.';
export const FORMATTING_WITHOUT_STARTER_CODE = 'Read the inputs from stdin solve the problem and write the answer to stdout (do not directly test on the sample inputs). Enclose your code within delimiters as follows. Ensure that when the python program runs, it reads the inputs, runs the algorithm and writes output to STDOUT.';

/** The official generic question template, plus how the answer is delivered in a workspace. */
export function lcbPrompt(p) {
  let prompt = `${SYSTEM_MESSAGE_GENERIC}\n\n### Question:\n${p.question_content}\n\n`;
  if (p.starter_code) {
    prompt += `### Format: ${FORMATTING_MESSAGE_WITH_STARTER_CODE}\n`;
    prompt += `\`\`\`python\n${p.starter_code}\n\`\`\`\n\n`;
  } else {
    prompt += `### Format: ${FORMATTING_WITHOUT_STARTER_CODE}\n`;
    prompt += '```python\n# YOUR CODE HERE\n```\n\n';
  }
  prompt += '### Answer\n\nWrite your complete solution to `solution.py` in this workspace (instead of a fenced code block). '
    + 'It is graded exactly as written in that file against hidden tests: '
    + (p.starter_code
      ? 'the `Solution` method from the starter code is called with each test\'s arguments and its return value is compared with the expected value.\n'
      : 'the program runs with each test\'s input on stdin and its stdout is compared with the expected output.\n');
  return prompt;
}

/** The template file: the starter code, or the official placeholder. */
export function templateSource(p) {
  return p.starter_code ? `${p.starter_code}\n` : '# YOUR CODE HERE\n';
}

export function lcbEnv() {
  return { ...offlineEnv([path.join(T.lcb, 'bin')]), CCEVAL_LCB_SRC: path.join(T.lcb, 'src') };
}

export function hiddenFiles(task) {
  return [{ rel: TESTS_REL, abs: path.join(LCB_DIR, task.id, 'tests', 'tests.json') }];
}

/** Score: official grader in a fresh copy under the network-denying sandbox. */
export async function lcbScore(task, workspace, scoreDir, outDir, tag, { solutionOverride } = {}) {
  const res = await scoreInCopy({
    workspace, scoreDir, outDir, hiddenFiles: hiddenFiles(task),
    setup: solutionOverride ? (dir) => solutionOverride(dir) : undefined,
    command: [py(T.lcb), '-I', GRADER, '--solution', 'solution.py', '--tests', TESTS_REL, '--timeout', String(LCB.timeoutPerTest)],
    env: lcbEnv(),
    // check_correctness's own global timeout is (timeout + 1) * n_tests + 5 s; allow interpreter start-up on top.
    timeoutMs: ((LCB.timeoutPerTest + 1) * task.n_tests + 5 + 120) * 1000,
    tag: tag.replace(/[^A-Za-z0-9_.-]/g, '_'),
    denyRead: [HIDDEN, SRC, ...LOCAL_REFS], denyWrite: [T.lcb, T.uvPython],
  });
  return res;
}

export const livecodebench = makeAdapter({
  name: 'livecodebench',
  title: 'LiveCodeBench (code generation, release_v6 window, Python)',
  tries: 1,
  defaultTimeoutMs: 30 * 60000,
  // --since/--until select problems by contest date (YYYY-MM-DD, inclusive).
  select(list, opts) {
    return list.filter((t) => (!opts.since || t.date >= opts.since) && (!opts.until || t.date <= opts.until));
  },
  hiddenFiles,
  score: (task, ws, scoreDir, outDir, tag) => lcbScore(task, ws, scoreDir, outDir, tag),
  agentPath: () => [path.join(T.lcb, 'bin')],
  aiderFiles: () => ({ fnames: ['solution.py'], ignore: [] }),
  reportNote: 'Contamination: every problem predates the model (contest dates 2024-09-22 .. 2025-04-06; Qwen3.8-27B was published 2026-08; its training cutoff is not published), so no post-cutoff window exists in release_v6 and these scores cannot exclude training-set exposure. Graded by LiveCodeBench\'s own `check_correctness` (all public + private tests, 6 s per test).',
});
