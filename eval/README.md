# CortexCode eval harness

Benchmarks that drive the real product headless: a frame with the real plugins and the local model,
one `chatflow.turn` per task message, and the benchmark's own grading. Baselines (aider,
mini-swe-agent, Harbor's Terminus-2) run on the same model through the model plugin's
OpenAI-compatible endpoint.

```
node eval/prepare.mjs [--suite <name>|all]          # datasets, task stores, toolchains (network)
node eval/run.mjs --suite <name> --model <variant>  # runs; resumable (finished tasks are kept)
node eval/report.mjs                                 # eval/results/REPORT.md from raw logs only
npm --prefix eval test                               # unit tests of the pure parts
```

Suites: `editbench`, `polyglot`, `realistic`, `livecodebench`, `bigcodebench`, `rustevo`,
`webbench` (native, on macOS) and the container suites `swebench` and `terminalbench` (below).
While the app is open, CortexCode runs drive its frame instead of a hidden one, so every task shows
as a conversation in the window (the app's config, one model loaded). A run that overrides the
product (`--model` other than the default variant, `--edit-format`) starts its own frame;
`CORTEX_EVAL_ATTACH=0` always does, `=1` refuses to run without the app. The real-model gates in
`plugins/agents/verify` attach the same way.

CortexCode runs as it is, the research plugin included (learning a task's domain online is part of how
it works). Polyglot runs are offline for the model since 2026-10-16: the harness disables the research plugin while attached and every workspace carries the tools sandbox's `.cortex-offline` marker (no outbound network for `run`), because the exercises and their tests are public and a model was seen cloning the exercise repository to read them. The marker also lists the hidden-test, exercise-source and reference-repository folders: neither the model's commands (sandbox `deny file-read*`) nor its `read` tool can open them (a model was seen running `find /` for the test file). CortexCode tasks have no time limit unless `--timeout-min N` sets one (a task ends when the agent
ends it, or when it is stopped in the window); baselines keep their suite's limit.

`node eval/run.mjs` without arguments prints the options; `run.mjs` and `prepare.mjs` document them
in their headers.

`knowledge` (no prepare step; docs/PROJECT-KNOWLEDGE-PLAN.md P7) measures whether what a project
learns reaches the next session and what it costs: 7 scenarios in `suites/knowledge/bank.json` on
the bowling fixture (`suites/knowledge/fixtures/bowling`), each a conversation whose later turn may
start in a new session of the same project (`new_session`); cards a scenario says to keep are
answered through `chatflow.knowledge.answer` / `chatflow.decision.answer` as a person would, seeds
are written through `memory.knowledge.upsert` before the first turn, and scoring is structural
(journals, the graph through `memory.knowledge.list`, the workspace). CortexCode only, 3 runs by
default; per-run `summary.md` and `results.jsonl`.

```
node eval/run.mjs --suite knowledge --model <variant> [--runs 3] [--tasks K1,K4]
```

`skillsel` (no prepare step) measures skill selection instead of task success: which skills each
turn reads (or a decision chooses and the host injects), when, and what they cost, over 66
scenarios in `suites/skillsel/bank.json` on the tiny projects in `suites/skillsel/fixtures/`
(design: `docs/SKILL-SELECTION-TESTS.md`). CortexCode only, 3 runs by default:

```
node eval/run.mjs --suite skillsel --model <variant> [--runs 3] [--group negative] [--tasks P1,C2]
node eval/suites/skillsel/compare.mjs eval/results/skillsel/cortex/<a> eval/results/skillsel/cortex/<b>
```

Skill names in the bank are data; the bank refuses to load a URI missing from
`plugins/lang-*/data/skills` or `plugins/skills-*/data/skills`. Each run directory also gets
`summary.md` (one row per turn) and `results.jsonl` (one row per turn as it ends).

Results: `eval/results/<suite>/<system>/<model>/run-<n>/` holds `run.json`, `frame.toml`, the
frame and plugin logs, and per task `tasks/<id>/result.json` with `attempt-<k>/` (prompt, journal,
diff, integrity check, `score/` with the grader's raw output).

## Container suites

Both run every task in its official Linux container and grade it with the benchmark's own
harness. CortexCode reaches the container through its container plugin (`container@1`,
`protocol/src/contracts/container.rs`): `container.attach` with the task image, the workdir and
`seed_from_image: true` copies the image's workdir to an empty host workspace and bind-mounts it
back, so CortexCode reads and edits the files on the host (code graph, edit tools) while its
commands run inside the container. The harness checks that the frame serves `container@1` before
each task and fails the task with that reason if it does not.

Requirements: Docker (Docker Desktop on macOS; the harness uses the active docker context's socket
and the user's docker config), and the container plugin in `cortex.dev.toml` for `--system cortex`.
The harnesses' Python packages live in uv venvs under `eval/cache/toolchains` (uv-managed CPython
3.12, hashed lock files `suites/*/requirements.lock`); system Python and Homebrew are never touched.

### SWE-bench Verified (`--suite swebench`)

- Dataset: `princeton-nlp/SWE-bench_Verified` at revision
  `c104f840cc67f8b6eec6f759ebc8b2693d585d4a` (test split, 500 instances; parquet SHA-256 pinned).
- Harness: `swebench==4.1.0`. Grading is `run_evaluation`'s per-instance path (`make_test_spec`,
  `build_env_images` for local images, `run_instance`, `report.json`) called from
  `suites/swebench/evaluate.py`, because the CLI cannot pass `arch` (it always builds x86_64 test
  specs). The verdict is the harness's `resolved`; one attempt per instance; the test run has
  `run_evaluation`'s default 1800 s timeout. An empty patch is unresolved without a container, as
  `run_evaluation` does.
- Why 4.1.0 and not 5.x: swebench 5.x grades a new dataset format (`SWE-bench/SWE-bench_Verified`,
  rows carrying `image` and `eval_script`), and its image builder only builds amd64 images. 4.1.0
  grades the princeton-nlp rows as published and builds env and instance images itself, for
  arm64 too. The two datasets have the same 500 instances; they differ only in the PASS_TO_PASS
  lists of `astropy__astropy-7606` and `django__django-10097`, checked on 2026-10-15.
- Prompt: the problem statement in SWE-agent's default `instance_template`
  (`config/default.yaml` at `3ea751c0`, formerly `anthropic_filemap.yaml`), with
  `working_dir = /testbed`. CortexCode keeps its own system prompt; only this user message is sent. `prepare` checks that the embedded template is that file's,
  verbatim. mini-swe-agent's own SWE-bench template is bash-tool specific, with a `cat patch.txt`
  submission step, so it does not fit a tool-calling agent.
- One task:
  1. pick the image (below) and make it available with the harness (`images.py`: pull, or build
     natively with `build_env_images` + `build_instance_image`, arch arm64);
  2. `container.attach {image, workdir: /testbed, seed_from_image: true, platform, env:
     BASH_ENV=/root/.bashrc}` onto an empty workspace; the harness checks the seeded repository is at
     the instance's `base_commit` and records what already differs from it;
  3. the prompt goes through `chatflow.turn`; the budget (60 min by default, `--timeout-min`)
     starts after attach;
  4. model patch = `git diff <base_commit>` of the workspace (tracked changes and new files,
     `.gitignore` respected, file modes ignored as the harness's own diffs do, private index),
     minus the files the instance's `test_patch` touches and minus files that differed right after
     seeding and that the agent did not change;
  5. `evaluate.py` grades it in the same image; `score/harness/logs/run_evaluation/...` keeps the
     harness's `run_instance.log`, `patch.diff`, `eval.sh`, `test_output.txt`, `report.json`;
  6. `container.detach`.
- Images on Apple Silicon (`--suite-opt images=auto|arm64|x86_64`):
  - `x86_64`: the published images `swebench/sweb.eval.x86_64.<id>` (all 500 exist; what every
    leaderboard run uses). Docker Desktop runs them under emulation.
  - `arm64`: native images. Docker Hub has published `swebench/sweb.eval.arm64.<id>` images for
    281 of the 500 instances (checked 2026-10-15 with registry manifest requests; per repository:
    django 141/231, sympy 67/75, sphinx 34/44, pytest 10/19, astropy 7/22, pylint 7/10, requests
    7/8, scikit-learn 4/32, matplotlib 2/34, seaborn 1/2, flask 1/1, xarray 0/22). For the others
    the harness builds the arm64 image locally (its Dockerfiles install aarch64 Miniconda); some
    environments may not build or may not reproduce the x86_64 test results on arm64.
  - `auto` (default): the arm64 image only where `prepare --images ... --validate` showed that the
    instance's gold patch resolves in it; otherwise the x86_64 image. Without a validation run,
    everything uses x86_64.
  - `suites/swebench/images.lock.json` pins the published images' manifest digests; each attempt
    records the image, its source, its repo digests and whether the digest matches the pin.
  - Opt-in preparation: `node eval/prepare.mjs --suite swebench --images all --validate
    [--only id,id] [--workers N]` pulls or builds the images and grades the gold patches,
    writing `eval/cache/hidden/swebench/images.json`.
- Baseline: mini-swe-agent's SWE-bench flow (`baselines/mini_swebench_runner.py`: the shipped
  `config/benchmarks/swebench.yaml`, `DefaultAgent`, docker environment on the same image,
  step_limit 250); its submission is graded as submitted. aider is not run: it has no shell and no
  environment, so it is not an agent harness for SWE-bench (`--system aider` is refused).

### Terminal-Bench 2.0 (`--suite terminalbench`)

- Dataset: `terminal-bench@2.0` as defined by the Harbor registry (`registry.json` at Harbor commit
  `f527f8e4`, SHA-256 pinned): 89 tasks of `laude-institute/terminal-bench-2` at
  `69671fbaac6d67a7ef0dfec016cc38a64ef7a77c`. `prepare` verifies the registry entry, exports the
  repository at that commit and copies each task directory into the task store.
- Harness: Harbor (`harbor==0.24.0`), TB 2.0's official harness. One task = one
  `harbor trial start -p <task>`: Harbor starts the environment, applies the task's agent timeout
  (`task.toml [agent].timeout_sec`), runs `tests/test.sh` in the same container afterwards (also
  after an agent timeout, and that reward counts), and writes `result.json`. Pass = reward 1; a
  trial without a reward counts as 0, as Harbor's mean metric counts it.
- CortexCode as a custom Harbor agent (`--agent cceval_tb:CortexAgent`) with the environment it
  needs (`--env cceval_tb:CortexContainerEnvironment`, `suites/terminalbench/harbor/cceval_tb.py`):
  1. the environment's `start()` pulls the task's prebuilt image (`--suite-opt build=native` builds
     `environment/Dockerfile` instead, Harbor's `--force-build`), reads the image's `WORKDIR`, and
     asks this harness, over a JSON-lines channel (fds 3 and 4), to `container.attach {image,
     workdir, seed_from_image: true, platform, env}`; the task's cpu and memory limits are then set
     on that container with `docker update`, as Harbor's docker environment sets them;
  2. the agent's `run()` hands Harbor's instruction to this harness, which sends it through
     `chatflow.turn`; when Harbor's agent timeout cancels `run()`, the cancellation is forwarded and
     the turn is cancelled before Harbor starts the verifier;
  3. Harbor's verifier uploads `/tests`, runs `test.sh` and copies `/logs/verifier` out through the
     environment's `exec` / `upload` / `download` (`docker exec` / `docker cp` on the attached
     container: the container plugin runs Docker and `ContainerInfo.id` is the container id);
  4. Harbor's `stop()` asks this harness to `container.detach`.
- Files outside the workdir: only the workdir is a bind mount. Everything else (installed
  packages, `/etc`, `/root`, `/protected`, `/tests`, `/logs`) lives in the container's own
  filesystem for the whole trial; the agent changes it through commands (which run in the
  container) and the tests see it there. CortexCode's file tools reach only the workdir.
- `--timeout-min` overrides the task's agent timeout (passed to Harbor as `--agent-timeout` and
  recorded); the harness's own deadline is only a backstop (Harbor's timeout plus 15 minutes).
- Raw logs: `tasks/<id>/harbor/<trial>/` (Harbor's `result.json`, `trial.log`, `verifier/`
  with `test-stdout.txt`, `reward.txt`, `ctrf.json`) and `tasks/<id>/harbor.log`.
- Baseline: Harbor's reference agent Terminus-2 (`--system terminus`,
  `baselines/tb_terminus_runner.py`) in Harbor's own docker environment, model
  `openai/<endpoint model>` with Terminus-2's `api_base` and `model_info` options.
- Images on Apple Silicon: the prebuilt TB 2.0 images are amd64 only (`alexgshaw/<task>:20251031`),
  so the default runs them under emulation. `build=native` builds the Dockerfiles for arm64; those
  Dockerfiles install packages at build time, so a native environment can differ from the
  published one.
- Opt-in control: `node eval/prepare.mjs --suite terminalbench --validate` runs Harbor's oracle agent
  (`solution/solve.sh`) on every task in Harbor's docker environment and records the rewards in the
  task manifest.

### Open points (container suites)

- Both suites need the container plugin (not in `cortex.dev.toml` yet). Its contract has no way to
  attach to a container someone else started, so Harbor's environment is backed by the container
  the plugin starts. It also has no cpu/memory fields (the harness applies TB limits with
  `docker update`) and no extra mounts, so Harbor's log directories are copied, not mounted.
- Prompts name container paths (`/testbed`, `/app/...`). Commands resolve them in the container;
  CortexCode's file tools must map them to the host root (the contract maps host paths to the
  container for `exec`, not the other way round).
- The workdir is a bind mount from macOS: file ownership and permissions inside it follow Docker
  Desktop's file sharing, and APFS is case-insensitive by default. Seeding differences are recorded
  per SWE-bench attempt (`environment.seed_differences`) and kept out of the model patch.
- Whether an instance's arm64 image builds and reproduces the reference results, and whether each
  TB task's oracle passes on this machine, is known only after the opt-in
  `prepare --images ... --validate` (SWE-bench) and `prepare --validate` (Terminal-Bench) runs.
