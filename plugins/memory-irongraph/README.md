# cc-memory — memory@1 on IronGraph

`cc-memory` is the CortexCode memory plugin (CPP/1, category `memory`, contract `memory@1`), built on
`irongraph-sdk =0.1.8`. It keeps one embedded IronGraph instance per process, the exclusive owner of
`<CORTEX_HOME>/irongraph` (`CORTEX_HOME`, else derived from `CORTEX_PLUGIN_DATA`). IronGraph's
embedding model lives in `<CORTEX_HOME>/irongraph-models` and its rebuildable vector cache in
`<CORTEX_HOME>/irongraph-cache` (`EmbeddedOptions::with_model_dir` / `with_embedding_cache_dir`, new
in 0.1.8). Nothing is read from or written to `~/.irongraph`.

One self-contained plugin folder (`plugin.toml`): `backend/` (the Cargo workspace: `src/`,
`tests/`, `examples/`), `ui/card-index/` (the card's Index section, `card.index@1`, an npm package
in the `ui/` workspace), `fixtures/` (shared by the tests and the verify scripts), `verify/` and
`bench/` (measurements). The frame loads it with `[[plugin]] dir = "plugins/memory-irongraph"`;
`cortex-pack build plugins/memory-irongraph` packs it into a `.ccplugin`.

## The contract boundary

Other plugins reach this one only through the named `memory@1` methods
(`protocol/src/contracts/memory.rs`): a question with typed parameters and a typed answer
(`memory.callers {sid}` -> call sites, `memory.files` -> paths). IronGraph, Cypher and the node
schema stay inside this folder, so another store can replace the plugin by implementing the same
contract. `memory.query` (read-only Cypher, `src/queries.rs`) is this plugin's own inspection
method for its tests, verify scripts and `cx call`; it is not part of `memory@1`, and no other
plugin calls it. A caller that needs something new gets a new named method here.

What the model needs to know about the store ships with it: `data/skills/irongraph-queries.md`
(when Cypher beats the graph:// URIs, the layers, the schema, query shapes) is this plugin's own
skill, listed by `memory.skills` and read as `skill://memory/irongraph-queries` (`src/skills.rs`,
the skills format of `cc-lang-kit`; `[backend] assets = ["data"]`). Another memory plugin brings
its own skills or none, so a swap never leaves the model a schema that is gone.

## Embedding engine (irongraph-sdk 0.1.8)

IronGraph 0.1.8 replaced the 1B Nemotron encoder with **Bekko a8m** (`hotchpotch/bekko-embedding-v1-a8m`
at revision `c721113d…`, normalized 384-dimensional vectors, Metal when available, else CPU). Its
three files total 246,317,545 bytes (`model.safetensors` 211,952,896, `tokenizer.json` 34,363,442,
`config.json` 1,207).

0.1.8 downloads the model on first open but, unlike 0.1.7, no longer verifies it (its checksum code
was removed) and gives an embedding application no progress. The plugin therefore installs the model
itself before it opens the database (`src/engine.rs`):

- every file is pinned to the size and digest Hugging Face publishes for that revision (SHA-256 of
  the LFS object; git blob SHA-1 for `config.json`) and re-verified on every start; a file that does
  not match is never handed to the SDK;
- the first start of a fresh `CORTEX_HOME` is the one-time install download: HTTPS from Hugging Face
  into `<file>.part`, resumed with an HTTP range request after an interruption (in-process retries,
  and across restarts), then verified and renamed into place;
- a verified copy in another model directory is cloned instead of downloaded: the directories in
  `CORTEX_IRONGRAPH_MODEL_SOURCE` (colon-separated), then `~/.cortexcode/irongraph-models`;
- progress and state: `memory.engine.status` (answered without the database, so it works during the
  download) and `memory.engine` events (`EngineStatus`: `installing | loading | ready | error`,
  `bytes_done / bytes_total`, `downloaded_bytes`, `resumed_from`, `source`, `install_ms`, `open_ms`),
  at most every 500 ms while downloading; the probe reports the model as its own check.

UI: `ui/card-index` (`card.index@1`, the index state). The knowledge items have no card section
(`ui/card-knowledge` is kept in the folder but not registered).

Once installed, everything works offline.

## IronGraph projects and layers

| Project | Holds |
|---|---|
| `main` | settings (`Settings -SECTION-> SettingsSection -HAS-> Setting`), global rules and style, the project registry (`ProjectEntry`), global notes |
| `cc_p_<hash(project id)>` | one per repository: the code graph, its work state and its notes |
| `cc_w_<hash(project, path)>` | one per attached agent workspace (see below) |

Every statement names its layers (`USE LAYER … WRITE LAYER …`):

- **OBSERVED** — parsed code: `Repo`, `Directory`, `File`, `Module`, `Function`, `Method`, `Class`,
  `Struct`, `Trait`, `Interface`, `Enum`, `TypeAlias`, `Field`, `Variant`, `Const`, `Static`,
  `Variable`, `Param`, `Component`, `Prop`, `Route`, `IpcChannel`, `Command`, `ShellFunction`,
  `Test`, `EnvVar`, `Dependency`, `ExternalSymbol` (all carry `Code`; definitions also `Sym`), and the
  edges `CALLS {args, line, confidence}`, `INSTANTIATES`, `RENDERS {props}`, `USES_HOOK`, `READS`,
  `WRITES`, `RETURNS`, `THROWS`, `OF_TYPE`, `HAS_PARAM`, `EXTENDS`, `IMPLEMENTS`, `OVERRIDES`,
  `IMPORTS`, `EXPORTS`, `DEFINES`, `CONTAINS`, `HANDLES`, `REQUESTS {method, url}` (fetch/axios →
  FastAPI/Flask/axum/actix/Express routes), `INVOKES {channel}` (Electron/Tauri IPC), `RUNS`
  (npm/cargo/pyproject commands → scripts → functions), `READS_CONFIG`, `TESTS`, `DEPENDS_ON`.
- **KNOWLEDGE** — derived facts: model-written `Description` nodes (`DESCRIBES` a symbol; stale
  when the symbol's `body_hash` changes; they follow a symbol through renames), and cited notes:
  `Note` (`CITES` a `Source {url}`; see Notes).
- **WORKSPACE** — `Task` (with `ContractCheck` nodes, `HAS_SUBTASK`, `DEPENDS_ON`, `TOUCHES` code),
  `BoardMessage` (`REFERS_TO`), `Agent` (`WORKS_ON`), `Rule` (`CONSTRAINS` code; CORTEX.md items
  have source `cortex_md`), `Style`, `Run` (code `MODIFIED_IN` run), settings, registry.

## Indexing

- Files: the `ignore` crate's rules (`.gitignore`, git excludes); `node_modules`, virtualenvs
  (`pyvenv.cfg`) and build output (`CACHEDIR.TAG`) are skipped; CORTEX.md is rules, not code.
- Languages are plugins (`lang@1`, `docs/LANGUAGE-PLUGINS.md`): this plugin parses nothing itself.
  `src/langs.rs` lists the active language plugins (`frame.plugins.list`), asks each instance
  `describe` once, and gives a file to the first plugin that names it by exact file name, file-name
  pattern, extension, then `#!` interpreter. A plugin's files go to `<category>.extract` in batches
  (200 files / 8 MB, several plugins at once); this plugin adds content hash, size, line count and
  the text excerpt, and the symbol-id prefix comes from `describe` (the plugins keep the former
  names and prefixes, so ids did not change: `tests/plugin_ids.rs`). A file no plugin covers is
  text or binary; a file its plugin fails on (error, timeout, not UTF-8) is text with the failure
  as the `File` node's `note`; nothing is dropped. `File` nodes carry `extractor` /
  `extractor_version`. A `plugin.state` / `plugin.swapped` event about a language plugin (the
  plugin is subscribed to both) re-matches every open tree: files a plugin covers now, or whose
  plugin is another instance version, are extracted again. Unchanged files an earlier index
  extracted keep their nodes (and the tasks, rules and descriptions linked to them) while no plugin
  can extract them, e.g. when the language plugins are still starting. `memory.project.status`
  reports `language_plugins` (language -> plugin, version, files), `uncovered` (by extension),
  `extract_problems` (text or kept) and `plugin_problems`.
- Cold index: parallel reads (rayon), extraction by the language plugins, full resolution in memory, batched `UNWIND`
  statements (inline-map `MERGE` on `sid`; equality indexes declared after the first write with
  `CREATE INDEX <name> IF NOT EXISTS …`, new in 0.1.8: an existing name succeeds with zero updates.
  0.1.7 never returned from a re-declaration, so the plugin used to read `SHOW INDEXES` first and
  serialise declarations; `tests/index_ddl.rs` re-declares an existing index under a timeout and
  shows 0.1.8 rejects an unguarded duplicate at once).
- Per save: FSEvents (`notify`) with a 60 ms debounce; the changed files' subgraphs are replaced in
  one batched sync; edges are rewritten only for files whose outgoing edge set changed (hash stored
  on `File.edge_hash`), nodes only when their properties changed (`props_hash`). Stable ids
  `<lang>:<path>#<qname>`; a removed and an added definition with the same kind and body hash is a
  rename/move: the node is kept (its sid updated), so `TOUCHES`, `CONSTRAINS` and descriptions
  survive.
- Pages and stylesheets (`lang-web`: `html`, `css`, `scss`, `less`) are one language family with
  their own rule (`model::Index::resolve_web`), since a reference's name is the symbol's name
  (`.btn`, `#main`, `--brand`, `$ink`, `@ink`, keyframes): a page's classes and ids (and `@extend`
  targets) link to the rules named so in the file itself (its `<style>` blocks) and in the
  stylesheets it brings in (`<link href>`, `@import`, `@use`, `@forward`, transitively, by the
  imports' `local` path), all of them; else project-wide only when one file defines the key. `--x`,
  `$x`, `@x` and keyframes names read in CSS resolve in the file and that chain, else project-wide
  (nearest by directory); a page attribute's id (`for`, `href="#x"`) is the page's own element;
  mixin calls resolve like variables. Element rules (`button {}`) are never linked. Their imports
  are linked by `local` path (stylesheets, scripts, images, pages). `tests/extract_resolve.rs`
  covers it on `fixtures/web` (with negative controls: an unimported stylesheet's `.btn`, a class
  two unimported files define, element names in text and attributes).
- Resolution is structural: imports (JS/TS relative paths and tsconfig aliases, Python modules,
  Rust module paths and workspace crates, Go import paths by the root `go.mod` module or the
  package directory, Java packages and static imports, C/C++ `#include` by the including directory
  or path suffix), `self`/`this`/`super` (and a Go method's receiver), Java/C++ class scope for
  bare names (implicit `this`, then the enclosing namespace), C++ `ns::`/`T::` scopes, typed
  parameters, locals whose declaration names their type, `self.field` types, same file (same
  package for Go), then unique project names. A C/C++ call that reaches both a header prototype and
  its definition links the definition.
  `confidence`: `heuristic` for those; `ambiguous` for a call on an untyped receiver linked to the
  only project method of that name and arity (several candidates link none).
- Line store (outside the graph): per content hash, each line's `cc_protocol::hashline::tag` and
  byte offset, in `<CORTEX_PLUGIN_DATA>/linestore/<project>/`; served by `memory.lines`.
- Repository documents are indexed by default (docs/PROJECT-KNOWLEDGE-PLAN.md P1): the `markdown`
  plugin's `lang-markdown` backend (`lang@1`) covers `*.md`, `*.mdx`, `*.markdown`, `README`,
  `CHANGELOG`, `CONTRIBUTING`, `AGENTS` and `CLAUDE`; every heading is a `section` symbol (label
  `Code:Section`, not `Sym`, so sections stay out of the repository map and name lookups) with the
  heading path as `signature`, its own text as `code` and `doc`, nested by level (a section's span
  encloses its subsections). A backtick identifier in a section's prose is a `Mention` reference:
  `model::Index::resolve_mention` links it (`MENTIONS {line, confidence}`) to the one symbol of the
  project with that name, else the one with that qualified name; several or none link nothing.
  `memory.search` ranks sections with the code (kind `section`). `tests/docs.rs`.
- Third-party code is not mapped: only the project's own files are indexed, and `ExternalSymbol`
  nodes an earlier version wrote (its offline docs layer) are removed when a project opens.
- Semantic search is IronGraph's automatic embedding index (`semantic_nodes`) bound to `Sym` and
  `Description` nodes, after exact name / qualified name / sid matches. No embedder is added, so a
  faster IronGraph embedding engine arrives with a version bump (as Bekko a8m did in 0.1.8). `memory.project.status.embedding`
  reports whether the latest written symbols are already retrievable.
- Repository map (`memory.repo_map`, `src/queries.rs`): PageRank-ranked paths and signatures,
  breadth over files first, the longest prefix that fits `budget_tokens` as the model counts it
  (`model.count` of a system message holding the text, less an empty one; bytes when no model
  answers). Files the repository declares `linguist-vendored` or `linguist-generated` in its
  `.gitattributes` (or `.git/info/attributes`) are left out of the map, not out of the graph
  (`src/attributes.rs`, git's precedence rules).

## Workspaces (BRAINSTORM §4.3)

`memory.workspace.attach` indexes an agent's copy-on-write clone as **its own IronGraph project**
(option b). Measured with `verify/workspace_bench.mjs` (`bench/workspace.json`, frame crate, 15 files):
attaching the clone (a full cold index) took 358 ms and IronGraph had embedded it 7.5 s later
(identical texts re-use its embedding cache); the delta an overlay would ingest (3 changed files)
took 60 ms. The overlay is cheaper to ingest but every query would have to merge two graphs and mask
the files the workspace shadows, which IronGraph's graph procedures (`graph.shortestpath`,
`graph.pagerank`) cannot do across projects, so paths, repo maps and centrality would be wrong in a
workspace. A per-workspace project keeps every query exact; its cost is one cold index of the
clone (for a mid-sized application repository, the cold index measured in `bench/index.json`).
Queries with `workspace` set read that project; work state stays in the repository's project.

## Reference projects (docs/IDEATION-WORKFLOW.md part 2)

`memory.project.open {root, reference: true}` (project id optional) opens another repository on disk
read-only: its registry entry is written at once and the call answers `state: indexing`; the index
runs in the background like any project's, and queries for its id wait for it (the per-project
opening lock). Without an id the project keeps the id already registered for that root, else the id
the frame gives the root (`p_` + 16 hex of SHA-256), so opening it later as a session project reuses
the index (and makes it a session project). Every project has a short unique name, its folder name
(`A-Z a-z 0-9 . _ -`, `-2`, `-3`, ... on a clash; never a graph verb, `global` or `main`), stable
once given; `memory.project.list` lists them (`project_id, name, root, reference, state`). A
reference is never written to: its CORTEX.md is not imported as rules, and tasks, board posts,
agents, runs, rules and style for it are refused; file-system refresh still applies.

While a reference's background index runs, `memory.project.status` answers at once (`state:
indexing`) with the progress: `indexed_files` (files whose facts are in: read as text or binary,
or extracted, counted per finished `extract` batch) of `total_files` (the files the scan found; 0
until then); both are equal once an index is done. Callers that must not block on a project being
indexed (the tools plugin's cross-project reads) ask it first. `memory.project.close {project_id}`
(an id or a name) forgets a reference: its registry entry, its IronGraph project, attached
workspaces and line index (the repository is not touched); a session project is refused. A
reference closed while its background index runs loses its registry entry at once and the rest when
the index stops; opening it again before that withdraws the close (`cx project close <name>`).
`tests/reference.rs` covers it (nothing in the reference changes, every query method by id, names,
a missing root, the progress of a held index, closing it by name and while it is indexed, and
re-opening it).

## Notes (cited knowledge)

`memory.note.add | search | list | remove` keep cited notes (research findings: claim, verbatim
quote, source url, title, date, topic; also `source`, `question`, `by`) in the KNOWLEDGE layer:
`(:Note {id, name, topic, topic_key, claim, quote, url, title, date, source, question, by,
project_id, created_ms})-[:CITES]->(:Source {url, name, title, date})` (`src/notes.rs`). Notes are
global, in the `main` project, unless a `project_id` is given: then they live in that project's
IronGraph project (a registered project; a read-only reference refuses adds and removals, like
other work state). Topics compare by key (lowercase letters and digits, other runs one `-`). The id
is `n-` + 16 hex of SHA-256 of scope, topic key, url and quote, so adding a note again returns the
stored one; both nodes are `MERGE`d on what the id is made of (a KNOWLEDGE node needs its `name` at
creation: a note's is its quote's start, a source's its url), so concurrent writers leave one
node. Removing a note removes its source when no other note cites it.

Search is by meaning only, with IronGraph's automatic embedding index (`semantic_nodes`, the same
Bekko model as code search; no embedder is added): the scope's `Note` nodes (of one topic when
asked) are bound before `SEARCH` ranks all of them, so `score` is the embedding similarity and a
topic filter is exact. IronGraph embeds asynchronously, in write order; a note written a moment ago
is not ranked yet, and `pending` counts those (bound notes minus ranked ones) instead of hiding
them. Measured on a fresh home (Bekko a8m): three notes written together were not ranked at once
and all ranked 250 ms later; in `tests/notes.rs` a question sharing almost no words with its note
ranked it first at 0.44 against 0.15 for an unrelated note, and the unrelated note's own subject
ranked it first at 0.46 against at most 0.06 for the others.

`tests/notes.rs` covers ranking by meaning with a negative control, `limit`, the topic filter,
global and project scopes (the scope is part of the id), idempotent adds, validation, an unknown
project, and removal (a source still cited stays, an uncited one goes).

## Knowledge (what the harness knows about the project's world)

`memory.knowledge.upsert | get | list | search | link | unlink | withdraw | supersede | linked | brief`
(docs/PROJECT-KNOWLEDGE-PLAN.md P0, `src/knowledge.rs`) keep knowledge items in the KNOWLEDGE layer of
the project's IronGraph project (the global scope's in `main`): `(:Knowledge:<Kind> {id, name, text,
state, source, confidence, as_of, by, evidence, aliases, ...})`, kinds `Concept | Fact | Decision |
Hypothesis | Question`, each with the states its kind allows (a hypothesis is `open | supported |
refuted`, a question `open | answered`, the rest `asserted` / `accepted`; every kind can be
`superseded` or `withdrawn`). The id is `k-` + 16 hex of SHA-256 of kind, scope and the normalized
name, so an upsert of the same name updates the item (text, state, confidence, `as_of`; evidence,
aliases and links are added, never dropped). Links are edges with a `rel` property: `IMPLEMENTED_BY` /
`ABOUT` to `Code` (by sid, resolved from a sid, path, qualified or unique bare name, so renames keep
them), `DESCRIBED_IN` to a document `Section`, `SUPPORTED_BY` to a `Note`, `TESTED_BY` to a `Run`
(the edge lives in WORKSPACE), `ANSWERS` and `SUPERSEDES` to another item. Nothing is deleted:
withdrawn and superseded items keep their node and links as history, and only live items are
served by `search` (IronGraph's embedding, an exact name or alias match first), `linked` (the items
bound to given sids, or to a file or any symbol of given paths, each with its one-line rendering
`<kind>[ <state>] (<source>[, run <id>], <date>): <text>  knowledge://<id>`) and `brief` (accepted
decisions, facts with the user's first, concepts by how much code they bind, open questions, one
line each, the longest prefix within `budget_tokens` as the model counts them, bytes without a
model). `memory.knowledge.changed` is published after every write. `tests/knowledge.rs` covers it
on the real engine.

## Verification

`verify/*.mjs` run against the real frame (`CORTEX_FRAME_BIN`, else `../../frame/target/release/cortex-frame`,
built when needed) on a temporary `CORTEX_HOME`:
`fixture_truth.mjs` (G3), `real_repos.mjs` (G4), `incremental.mjs` (G5), `semantic.mjs` (G6),
`workstate.mjs` (G7), `settings.mjs` (G8), `upgrade_018.mjs` (G9), `workspace_bench.mjs` (measurement).
Results go to `bench/`. Every verification home is fresh, so `verify/lib.mjs` keeps one verified
model copy in `$TMPDIR/cc-memory-model-cache` and clones it into each new home (the plugin
re-verifies it); `tests/embedded.rs` does the same. The scripts' frame loads the language plugins'
folders beside the memory plugin (`verify/lib.mjs`, `LANG_PLUGINS`) and waits for them to be active.

The Rust tests that need code facts run the real language plugins: `tests/common` is an in-test
frame peer that answers `frame.plugins.list` and routes `lang-*` methods to the plugins' crates
(dev-dependencies, served in-process by the kit's `LangPlugin`, the object `run_pack` serves).
`tests/extract_resolve.rs` checks facts and resolution, `tests/plugin_ids.rs` compares the graph
rows from the plugins' facts with `fixtures/ids-before-plugins.json` (the in-process extractors'
rows, recorded before they were removed), and `tests/embedded.rs` covers plugins that start late,
stop, fail and come back.
