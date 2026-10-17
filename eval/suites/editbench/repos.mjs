// Source repositories for the edit-format benchmark, pinned by commit. Files are read with
// `git show <commit>:<path>`, so the working tree state of the clone never matters.
export const EDITBENCH_REPOS = [
  {
    id: 'excalidraw', lang: 'ts', url: 'https://github.com/excalidraw/excalidraw.git', dir: 'eb-excalidraw',
    commit: '4c37956ba5f04ec342a9d9d5891a38f22c5b7b4e',
    include: [/^packages\/(common|element|excalidraw|math|utils)\/.*\.(ts|tsx)$/],
  },
  {
    id: 'jotai', lang: 'ts', url: 'https://github.com/pmndrs/jotai.git', dir: 'rz-jotai',
    commit: 'dceab08751320d2bfd707e7a54a880db73d7a069',
    include: [/^src\/.*\.(ts|tsx)$/],
  },
  {
    id: 'flask', lang: 'python', url: 'https://github.com/pallets/flask.git', dir: 'eb-flask',
    commit: 'd086db856be187255b8ec61ef409357393020f32',
    include: [/^src\/flask\/.*\.py$/],
  },
  {
    id: 'httpx', lang: 'python', url: 'https://github.com/encode/httpx.git', dir: 'eb-httpx',
    commit: 'b5addb64f0161ff6bfe94c124ef76f6a1fba5254',
    include: [/^httpx\/.*\.py$/],
  },
  {
    id: 'click', lang: 'python', url: 'https://github.com/pallets/click.git', dir: 'rz-click',
    commit: '2247b35ea1c47c727d7a06e51fa280e12a863ff6',
    include: [/^src\/click\/.*\.py$/],
  },
  {
    id: 'werkzeug', lang: 'python', url: 'https://github.com/pallets/werkzeug.git', dir: 'eb-werkzeug',
    commit: '23496a7290fc3ca468a1ec558adf518ef225fc86',
    include: [/^src\/werkzeug\/.*\.py$/],
  },
  {
    id: 'ripgrep', lang: 'rust', url: 'https://github.com/BurntSushi/ripgrep.git', dir: 'eb-ripgrep',
    commit: '3fce3b5bb0236da2df6d99672afb8a719642eca7', rustEdition: '2024',
    include: [/^crates\/.*\/src\/.*\.rs$/],
  },
  {
    id: 'just', lang: 'rust', url: 'https://github.com/casey/just.git', dir: 'rz-just',
    commit: 'a6bc306dea12a00980fbf834560d786c690806e0', rustEdition: '2024',
    include: [/^src\/.*\.rs$/],
  },
];

// Structural path rules: test, fixture, generated and declaration files are not mutation targets.
export const EXCLUDE = [
  /(^|\/)(tests?|__tests__|__mocks__|fixtures|benches|examples|testdata|test-utils)\//,
  /\.(test|spec)\.[jt]sx?$/, /(^|\/)test_[^/]*\.py$/, /_test\.py$/, /(^|\/)conftest\.py$/,
  /\.d\.ts$/, /(^|\/)tests?\.rs$/,
];

export const LANGS = {
  ts: { label: 'TypeScript/React', types: ['operator_swap', 'boolean_flip', 'off_by_one', 'removed_optional_chain', 'removed_guard_clause', 'renamed_identifier'] },
  python: { label: 'Python', types: ['operator_swap', 'boolean_flip', 'off_by_one', 'removed_guard_clause', 'renamed_identifier'] },
  rust: { label: 'Rust', types: ['operator_swap', 'boolean_flip', 'off_by_one', 'removed_guard_clause', 'renamed_identifier'] },
};

/**
 * Tasks per language. 60 per language (180 total) matches Stencil's 180-task runs, so our numbers
 * sit on the same footing. With 3 runs per edit format, a format's pass rate near 0.7 has a Wilson
 * 95% half-width of about +/-6.7 points per run over 180 tasks (+/-3.9 over the 540 pooled
 * attempts). For paired McNemar comparisons over 540 task-run pairs, if formats disagree on 20-25%
 * of pairs, a difference of about 5-6 points is detected with 80% power at alpha 0.05 (normal
 * approximation: (1.96 + 0.84) * sqrt(discordant) / 540), before allowing for task clustering.
 */
export const TASKS_PER_LANG = 60;
export const SEED = 'cortexcode-editbench-v1';
export const MIN_LINES = 30;
export const MAX_LINES = 1500;
