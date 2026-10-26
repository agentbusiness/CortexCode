# CortexCode: notes for anyone working on this repository

CortexCode is a local coding agent: a Rust frame that runs everything as plugins (chat flow, model,
memory and code graph, tools, language plugins, skills, agents) and an Electron UI. Its model is a
small local one (Qwen3.8-27B, 4-bit, on uzu), offline. Design notes: `docs/BRAINSTORM.md`.

## The lens: design for the small model

**A small model doesn't follow rules; it follows the nearest strong cue at the moment it decodes.**

Every prompt, stage message, tool result, skill and flow decision is written with this in mind.

- **When the model does the wrong thing, find the cue it saw last** before that decision, and fix the
  cue: put the right one next to the decision, or take the wrong one out of its prompt. Do not answer
  with one more rule. Cues that misled it in real runs:
  - a stub's function name beat the task statement (it returned a bool for "compute an ordering");
  - a restated brief or work line placed just before the work beat the statement it summarised;
  - tool definitions in a prompt that could not run tools: it wrote tool calls as plain text;
  - a test log shown only head and tail: it stopped to ask for the rest instead of fixing;
  - a passing check script in view: it kept re-running it.
- **The text right before a decision wins.** Put the task's own statement, the files it touches and the
  whole evidence (test output) there; keep restatements of them out.
- **A message's last sentence is the step to take next.** Never end with "stop" or "reply" unless that
  is the step. A prompt that cannot use tools carries none.
- **Never hand the model all the data.** The prompt carries meaning, not code: the project's
  description (what the software is, written once from its documents), the project's own contract
  (its `AGENTS.md`) and what the project knows. No repository map and no generic reads. Code reaches
  the model only by targeted discovery inside a task: a semantic search for the task's topic, the
  files the task names, and the searches and reads the model asks for itself.
- **Plan first, then discover.** A change is planned from the request and the project description;
  each task then finds its own code.
- **Prompts are short numbered steps**, one reason per choice, with an example. No stacked clauses.
- **The host decides what it can decide from structural facts** (code side over model side); the
  model's steps stay small. Gates should let the model flow: prefer a call that runs with a note over
  a refusal that costs it a step; keep only refusals that protect work or safety.
- **What the host can repair, it repairs.** A call whose meaning is clear from its shape (a read of
  `run:<command>`, a duplicated line named by its shown anchor) is carried out, with a note, instead
  of failing and costing a step. A failure stays only where the model has to decide something.
- **The repository changes only on the user's word.** A git command that changes it (commit, add,
  checkout, restore, reset, stash, branch, merge, rebase, clean, push) waits for the user's approval
  of that command, in every mode; reading history runs at once.

## Rules for the model-facing text

- Never task- or domain-specific content: no benchmark exercise names, no solution hints. Examples are
  neutral (a CSV export, a cache).
- Tools: each description says what the tool is for and what to use instead (at most 100
  characters); the tools guide (`plugins/tools/backend/src/schemas.rs`) has the numbered "which tool"
  list and says how every result reads.
- Tool results carry only the answer: no headers that repeat the request, no counts, ids, sizes or
  timings nobody acts on, no internal jargon. A failure ends with its next step.
- Plans: the tasks are the steps (no second list); after each task a short REVIEW asks whether the
  tasks still to do fit what the work showed, and writes them again when not.
- Skills: line 1 is the catalog summary (when the skill helps, at most 100 characters), then a blank
  line, then concrete steps and commands; no worked scenario examples in procedural skills (the
  model copies their shape). See `docs/SKILLS-BRIEF.md`.
- Where things live: chat flow prompts `plugins/chatflow/backend/data/prompts/*.md`, short texts
  `plugins/chatflow/backend/data/texts.toml`, stage settings `plugins/chatflow/backend/data/stages.toml`,
  decision schemas `plugins/chatflow/backend/data/schemas/`, language guides
  `plugins/lang-*/data/skills/`, general skills `plugins/skills-*/data/skills/`, the memory
  plugin's own skills `plugins/memory-irongraph/data/skills/`.

## Plugin boundaries

- Plugins talk only through the contract methods in `protocol/src/contracts/`: a named question with
  typed parameters and a typed answer (`memory.callers {sid}`, `memory.files`). The frame routes a
  call by its method's prefix to the active plugin of that category.
- No direct queries across plugins: no Cypher, SQL or other query text in a call. The store, its
  query language and its schema belong to the plugin that owns them (IronGraph stays inside
  `plugins/memory-irongraph`), so a plugin can be replaced by another that implements the same
  contract.
- A caller that needs something the contract lacks gets a new named method in the contract and in
  the owning plugin. A plugin's own inspection methods (`memory.query`) serve its own tests and
  verify scripts, never other plugins.
- Know-how about a store's query language and schema is a skill of the plugin that owns the store
  (`plugins/memory-irongraph/data/skills/irongraph-queries.md`, `skill://memory/...`), never of a
  skills plugin, so it leaves with the store.

## Checking a change

- No tests, test harnesses or benchmarks in this repository: no unit tests (`#[cfg(test)]`,
  `*.test.*`), no `tests/` folders, no verify or check scripts, no eval or bench folders. Check a
  change by using the real app on a real project.
- The frame runs plugins only from installed packages (`~/.cortexcode/plugins/<id>/<version>/`),
  never from a plugin folder: a cargo or npm build alone does not reach the app. Deliver a changed
  plugin with `packer/target/release/cortex-pack build plugins/<name> --install` (a dev build; the
  running app swaps to it live, backend and UI together, and the plugin's data folders are copied
  into the code folder `~/.cortexcode/code/plugins/<name>/`, where plugins read prompts, texts and
  scripts live); `cx plugin rollback <name>` undoes the swap.
- One CortexCode runs at a time: the home lock refuses a second frame, and a frame stops the
  processes a dead predecessor left (`<home>/run/children.json`) before it starts its own.
- When the model does the wrong thing, read its steps (the session's journal) before changing
  anything: the fix goes to the mechanism (stage, schema, cue), not to wording.
- No locks or quiet waits in code.

## Releases

- Everything is built on the owner's Mac (Linux in Docker on it), never on GitHub; files go to the
  repository's GitHub releases and the catalog (release `catalog`) names what is current.
  Guide: `README.md` "Releasing"; design: `docs/RELEASES.md`.
- Each plugin is released on its own (`scripts/release-plugin.sh <plugin>` or `--changed`); the
  frame (`scripts/release-frame.sh`), the full app (`scripts/release-app.sh`) and the npx launcher
  (`scripts/release-npm.sh`) each on their own. Versions: three numbers, each 0 to 99.
- The app updates itself through `plugins/updater` (`update@1`): plugins the user did not change
  install when no run is going, changed ones ask first, a new frame waits for Restart to update
  (`frame/src/update.rs`: staged frames in `<home>/frame/<version>/`, the app's own frame is the
  safe copy).
- The release scripts change only version lines; committing them is the owner's call.
