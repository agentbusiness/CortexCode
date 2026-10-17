// BigCodeBench-Hard (v0.1.4, 148 library-heavy Python tasks) in both official splits:
//   complete: the workspace's solution.py holds the function signature and docstring to complete;
//   instruct: the natural-language instruction, solution.py starts empty.
// The agent edits solution.py; BigCodeBench's own checker runs the task's unittest TestCases under
// its official limits, inside the network-denying sandbox, with libraries from a uv venv in
// eval/cache/toolchains/bcb (pass@1, one attempt).
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeAdapter } from '../../lib/adapter.mjs';
import { LOCAL_REFS, HIDDEN, SRC, TOOLCHAINS } from '../../lib/paths.mjs';
import { scoreInCopy } from '../../lib/score.mjs';
import { offlineEnv, py, T } from '../../lib/toolchains.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));

export const BCB = {
  dataset: 'bigcode/bigcodebench-hard',
  datasetRevision: '298d2cc7b96612e15e47313c3603ee124cee0c1f',
  file: { name: 'data/v0.1.4-00000-of-00001.parquet', sha256: '73a4270b43feb81abefad7bb1b592937c768ea45c3263bedc1cabd30a9a6ce79', version: 'v0.1.4' },
  repo: 'https://github.com/bigcode-project/bigcodebench.git',
  commit: '09dd993f46c3fbf3a799465bb96d524edcb0b199',
  checkerFiles: ['bigcodebench/__init__.py', 'bigcodebench/eval/__init__.py', 'bigcodebench/eval/utils.py', 'bigcodebench/eval/_special_oracle.py'],
  python: '3.10.18',
  requirements: path.join(HERE, 'requirements-eval-macos.txt'),
  lock: path.join(HERE, 'requirements-eval-macos.lock'),
  // bigcodebench/generate.py defaults.
  instructionPrefix: 'Please provide a self-contained Python script that solves the following problem in a markdown code block:',
  splits: ['complete', 'instruct'],
  // NLTK data the official image fetches at run time (nltk.download inside task code needs the
  // network); pinned from github.com/nltk/nltk_data gh-pages.
  nltkCommit: '550b6625bcef1f2abff2ff770a5a0d272c9c6b2a',
  nltkPackages: [
    { path: 'tokenizers/punkt', sha256: '51c3078994aeaf650bfc8e028be4fb42b4a0d177d41c012b6a983979653660ec' },
    { path: 'corpora/stopwords', sha256: '48c0e52d8b52546e827f53761fb30300c0ab94f70660d28bd65ba0a86270946b' },
  ],
};

export const BCB_DIR = path.join(HIDDEN, 'bigcodebench');
export const BCB_MPLCONFIG = path.join(TOOLCHAINS, 'bcb-mplconfig');
export const GRADER = path.join(HERE, 'grade.py');
export const TASK_REL = '.cceval_bcb_task.json';

/** The official chat prompt (provider/utility.py make_raw_chat_prompt) plus workspace delivery. */
export function bcbPrompt(split, row) {
  let p;
  if (split === 'complete') p = `${BCB.instructionPrefix}\n\`\`\`\n${row.complete_prompt.trim()}\n\`\`\`\n`;
  else p = `${BCB.instructionPrefix}\n${row.instruct_prompt.trim()}\n`;
  p += '\nWrite the complete self-contained script to `solution.py` in this workspace (instead of a markdown code block)'
    + (split === 'complete' ? '; it already holds the imports, signature and docstring to complete' : '')
    + '. It is graded exactly as written in that file by hidden unit tests.\n';
  return p;
}

export function bcbEnv() {
  return {
    ...offlineEnv([path.join(T.bcb, 'bin')]),
    CCEVAL_BCB_SRC: path.join(T.bcb, 'src'),
    // The official image is headless Linux, where matplotlib uses Agg; macOS would default to the GUI backend.
    MPLBACKEND: 'Agg',
    NLTK_DATA: path.join(T.bcb, 'nltk_data'),
    HF_HUB_OFFLINE: '1',
    // matplotlib's font cache: built once at prepare time, writable (matplotlib refuses a read-only
    // config dir and would rebuild the cache, ~30 s, on every run), outside the user's home.
    MPLCONFIGDIR: BCB_MPLCONFIG,
  };
}

export function hiddenFiles(task) {
  return [{ rel: TASK_REL, abs: path.join(BCB_DIR, task.id, 'tests', 'task-test.json') }];
}

// Some tasks' tests use fixed absolute paths (e.g. /tmp/archive), so the two splits of one
// BigCodeBench task must never be scored at the same time (the official harness evaluates one split
// per run). Scoring is serialized per BigCodeBench task id within this process.
const scoringLocks = new Map();
async function withTaskLock(key, fn) {
  const prev = scoringLocks.get(key) ?? Promise.resolve();
  let release;
  const mine = new Promise((r) => { release = r; });
  const chained = prev.then(() => mine);
  scoringLocks.set(key, chained);
  await prev;
  try { return await fn(); } finally {
    release();
    if (scoringLocks.get(key) === chained) scoringLocks.delete(key);
  }
}

export async function bcbScore(task, workspace, scoreDir, outDir, tag, opts = {}) {
  return withTaskLock(task.bcb_task_id, () => bcbScoreUnlocked(task, workspace, scoreDir, outDir, tag, opts));
}

async function bcbScoreUnlocked(task, workspace, scoreDir, outDir, tag, { solutionOverride } = {}) {
  return scoreInCopy({
    workspace, scoreDir, outDir, hiddenFiles: hiddenFiles(task),
    setup: solutionOverride ? (dir) => solutionOverride(dir) : undefined,
    command: [py(T.bcb), '-I', GRADER, '--solution', 'solution.py', '--task', TASK_REL],
    env: bcbEnv(),
    timeoutMs: (241 + 1 + 120) * 1000,
    tag: tag.replace(/[^A-Za-z0-9_.-]/g, '_'),
    denyRead: [HIDDEN, SRC, ...LOCAL_REFS], denyWrite: [T.bcb, T.uvPython],
  });
}

export const bigcodebench = makeAdapter({
  name: 'bigcodebench',
  title: 'BigCodeBench-Hard v0.1.4 (Python, library-heavy; complete and instruct splits)',
  tries: 1,
  defaultTimeoutMs: 30 * 60000,
  hiddenFiles,
  score: (task, ws, scoreDir, outDir, tag) => bcbScore(task, ws, scoreDir, outDir, tag),
  agentPath: () => [path.join(T.bcb, 'bin')],
  agentEnvExtra: () => ({ MPLBACKEND: 'Agg', NLTK_DATA: path.join(T.bcb, 'nltk_data'), MPLCONFIGDIR: BCB_MPLCONFIG }),
  aiderFiles: () => ({ fnames: ['solution.py'], ignore: [] }),
  reportNote: 'Graded by BigCodeBench\'s own `untrusted_check` (calibrated, official limits) in a macOS arm64 CPython 3.10 venv; library pins that have no macOS arm64 wheel were moved to the nearest available version (suites/bigcodebench/requirements-eval-macos.txt lists each). Tasks whose canonical solution fails offline are excluded (see the suite manifest).',
});
