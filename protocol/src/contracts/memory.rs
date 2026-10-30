//! `memory@1`: project memory (code graph, line store, work state, settings, notes, knowledge).
//!
//! Callers name a question and get a typed answer; no method takes a query language, so the store
//! behind the contract is the plugin's choice (the shipped plugin, `memory-irongraph`, keeps it on
//! embedded IronGraph). One graph per repository: files, functions, classes, params, routes,
//! components, dependencies, tasks, agents, board, rules and user style, except the line-level
//! index (line numbers, per-line hashes, byte offsets), which the plugin keeps in its line store.
//! Cited notes (research findings) are global unless a project is named.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The contract name.
pub const CONTRACT: &str = "memory@1";

// The frame's records (projects, sessions, settings, old journals) are kept through this contract;
// their methods and types live with the frame's interface (`contracts::records`).
pub use super::records::{
    AppDeletedResult, AppProjectDeleteParams, AppProjectPutParams, AppProjectsResult, AppSessionPutParams, AppSessionsDeleteParams, AppSessionsResult,
    JournalDropParams, JournalReadParams, JournalReadResult, SettingsGetParams, SettingsSetParams,
};

pub mod methods {
    // The methods the frame keeps its records through (`contracts::records`), part of this contract.
    pub use crate::contracts::records::methods::*;

    /// Open (and index on first open) the project for a repository root. With `reference: true`
    /// the root is another repository opened read-only: registered at once, indexed in the
    /// background (queries wait for the index), never written to.
    pub const PROJECT_OPEN: &str = "memory.project.open";
    /// `{}` -> [`super::ProjectListResult`]: every known project (session projects and read-only
    /// references) with its short unique name, for `@project` mentions and URI qualifiers.
    pub const PROJECT_LIST: &str = "memory.project.list";
    /// `{project_id}` -> [`super::ProjectStatus`]. While a reference opened with
    /// [`PROJECT_OPEN`] is still being indexed in the background it answers at once (`state:
    /// indexing`, progress in `indexed_files` of `total_files`) instead of waiting for the index.
    pub const PROJECT_STATUS: &str = "memory.project.status";
    /// [`super::ProjectCloseParams`] -> [`super::ProjectCloseResult`]: forget a read-only reference
    /// project: its registry entry, its code graph and line index (the repository on disk is not
    /// touched). Session projects are refused. A reference still being indexed is forgotten once
    /// its index stops.
    pub const PROJECT_CLOSE: &str = "memory.project.close";
    /// Re-index changed files now (the plugin also watches the file system itself).
    pub const REFRESH: &str = "memory.refresh";
    /// Index a workspace (an agent's clone) as an overlay of the project.
    pub const WORKSPACE_ATTACH: &str = "memory.workspace.attach";
    pub const WORKSPACE_DETACH: &str = "memory.workspace.detach";

    pub const SEARCH: &str = "memory.search";
    pub const SYMBOL: &str = "memory.symbol";
    pub const CALLERS: &str = "memory.callers";
    pub const CALLEES: &str = "memory.callees";
    pub const IMPACT: &str = "memory.impact";
    pub const PATH: &str = "memory.path";
    pub const OUTLINE: &str = "memory.outline";
    /// [`super::FocusParams`] -> [`super::FocusResult`]: the definitions and document sections of
    /// one file, each scored by how well it matches a text (what a read is for).
    pub const FOCUS: &str = "memory.focus";
    pub const REPO_MAP: &str = "memory.repo_map";
    /// [`super::FilesParams`] -> [`super::FilesResult`]: the repository-relative paths of every
    /// indexed file (code, documents, text and binary), sorted.
    pub const FILES: &str = "memory.files";
    /// `{}` -> [`super::super::skills::Description`]: the plugin's own skills (know-how for the
    /// model about its store, e.g. how to query it). They go into the skill catalog with the
    /// skills plugins' and are read as `skill://memory/<name>`; a plugin without skills lists none.
    pub const SKILLS: &str = "memory.skills";
    /// [`super::super::skills::SkillParams`] -> [`super::super::skills::SkillResult`]: one skill's text.
    pub const SKILL: &str = "memory.skill";
    /// Store what a symbol does on its node (`description`, stale once its body changes).
    pub const DESCRIBE: &str = "memory.describe";
    /// Symbols that still need a description, most central first.
    pub const UNDESCRIBED: &str = "memory.undescribed";

    pub const TASK_CREATE: &str = "memory.task.create";
    pub const TASK_UPDATE: &str = "memory.task.update";
    pub const TASK_GET: &str = "memory.task.get";
    pub const TASK_LIST: &str = "memory.task.list";

    pub const BOARD_POST: &str = "memory.board.post";
    pub const BOARD_LIST: &str = "memory.board.list";

    pub const AGENT_UPSERT: &str = "memory.agent.upsert";
    pub const AGENT_LIST: &str = "memory.agent.list";

    pub const RULE_LIST: &str = "memory.rule.list";
    pub const RULE_UPSERT: &str = "memory.rule.upsert";
    pub const STYLE_LIST: &str = "memory.style.list";
    pub const STYLE_UPSERT: &str = "memory.style.upsert";

    /// Record a finished run (trajectory summary, outcome) for learning.
    pub const RUN_RECORD: &str = "memory.run.record";

    /// Line-level index of a file from the line store: per-line hashline tags and byte offsets.
    pub const LINES: &str = "memory.lines";

    /// The settings under a prefix as nested JSON (`""` = everything).
    pub const SETTINGS_LIST: &str = "memory.settings.list";
    /// Delete one setting, or a whole section subtree.
    pub const SETTINGS_DELETE: &str = "memory.settings.delete";
    /// IronGraph engine state: embedding-model installation (progress, resumable download) and
    /// readiness. Answered without opening the database, so it works while the model installs.
    pub const ENGINE_STATUS: &str = "memory.engine.status";

    /// Cited notes (claim, verbatim quote, source url, title, date, topic) in IronGraph's
    /// KNOWLEDGE layer: global (the main project) unless a `project_id` is given, then that
    /// project's (a read-only reference refuses them).
    /// [`super::NoteAddParams`] -> [`super::Note`]. Idempotent: the same scope, topic, url and
    /// quote give the same note id, and adding it again returns the stored note.
    pub const NOTE_ADD: &str = "memory.note.add";
    /// [`super::NoteSearchParams`] -> [`super::NoteSearchResult`]: by meaning (IronGraph's own
    /// embedding of the notes), optionally only one topic; best first, with `score`.
    pub const NOTE_SEARCH: &str = "memory.note.search";
    /// [`super::NoteListParams`] -> [`super::NoteListResult`]: a topic's notes (every topic's when
    /// absent), oldest first.
    pub const NOTE_LIST: &str = "memory.note.list";
    /// [`super::NoteRemoveParams`] -> [`super::NoteRemoveResult`]; a source no note cites any more
    /// goes with it.
    pub const NOTE_REMOVE: &str = "memory.note.remove";

    // ---- project knowledge (docs/PROJECT-KNOWLEDGE-PLAN.md) --------------------------------------
    // What the harness knows about a project's world, kept in the project's KNOWLEDGE layer (or the
    // global scope without `project_id`): concepts, facts, decisions, hypotheses and questions, each
    // with its source, state, evidence and links to code, notes, sections and runs.
    /// [`super::KnowledgeUpsertParams`] -> [`super::KnowledgeItem`]. The id is made of the kind, the
    /// scope and the normalized name, so writing an item again updates it (text, state, confidence,
    /// `as_of`; evidence, aliases and links are added, never dropped) instead of making a twin.
    pub const KNOWLEDGE_UPSERT: &str = "memory.knowledge.upsert";
    /// [`super::KnowledgeGetParams`] -> [`super::KnowledgeItem`] (`not_found` otherwise).
    pub const KNOWLEDGE_GET: &str = "memory.knowledge.get";
    /// [`super::KnowledgeListParams`] -> [`super::KnowledgeListResult`]: newest first; withdrawn and
    /// superseded items only when asked.
    pub const KNOWLEDGE_LIST: &str = "memory.knowledge.list";
    /// [`super::KnowledgeSearchParams`] -> [`super::KnowledgeSearchResult`]: live items by meaning
    /// (IronGraph's own embedding), an exact name or alias match first; best first, with `score`.
    pub const KNOWLEDGE_SEARCH: &str = "memory.knowledge.search";
    /// [`super::KnowledgeLinkParams`] -> [`super::KnowledgeItem`]: add one link (see
    /// [`super::KnowledgeLink`]); a target that does not resolve is `not_found`.
    pub const KNOWLEDGE_LINK: &str = "memory.knowledge.link";
    /// [`super::KnowledgeLinkParams`] -> [`super::KnowledgeItem`]: remove one link.
    pub const KNOWLEDGE_UNLINK: &str = "memory.knowledge.unlink";
    /// [`super::KnowledgeWithdrawParams`] -> [`super::KnowledgeItem`]: state `withdrawn` with the
    /// reason; the node and its links stay (history), and search, `linked` and the brief skip it.
    pub const KNOWLEDGE_WITHDRAW: &str = "memory.knowledge.withdraw";
    /// [`super::KnowledgeSupersedeParams`] -> [`super::KnowledgeItem`] (the superseded one): the old
    /// item's state becomes `superseded`, `superseded_by` names the new one, and the new one
    /// `SUPERSEDES` it.
    pub const KNOWLEDGE_SUPERSEDE: &str = "memory.knowledge.supersede";
    /// [`super::KnowledgeLinkedParams`] -> [`super::KnowledgeLinkedResult`]: the live items bound to
    /// the given code (by sid, or any symbol of the given paths), each with its one-line rendering.
    /// What rides along with code when the host serves it; no model, no search.
    pub const KNOWLEDGE_LINKED: &str = "memory.knowledge.linked";
    /// [`super::KnowledgeBriefParams`] -> [`super::KnowledgeBriefResult`]: what every session of the
    /// project should know, one line per item, within a token budget: accepted decisions, user
    /// facts, concepts by how much code they bind, open questions.
    pub const KNOWLEDGE_BRIEF: &str = "memory.knowledge.brief";
    /// [`super::KnowledgeCountParams`] -> [`super::KnowledgeItem`]: add to an item's `seen`,
    /// `helped` and `hurt` counters (how a lesson fared where it was shown).
    pub const KNOWLEDGE_COUNT: &str = "memory.knowledge.count";

    /// Append a session's journal patches, in order, to its stream: `{session_id, patches:
    /// [JournalPatch]}` -> `{next}` (the offset after the last one). Frames before the move of
    /// journals into files wrote here; the frame no longer calls it.
    pub const JOURNAL_APPEND: &str = "memory.journal.append";

    // Security findings (the security plugin's audits, kept in the project's graph):
    /// Create or replace an audit run's record: `{project_id, audit: SecurityAudit}` -> `{}`.
    pub const SECURITY_AUDIT_PUT: &str = "memory.security.audit.put";
    /// Create or update findings by id (their fingerprint): `{project_id, audit_id, findings:
    /// [SecurityFinding]}` -> `{findings: [SecurityFinding]}` as stored. A finding seen before keeps
    /// its status and history; one that was fixed, verified or gone and is seen again is open again.
    pub const SECURITY_FINDINGS_PUT: &str = "memory.security.findings.put";
    /// The project's findings, highest priority first: `{project_id, status?: [status], class?,
    /// min_priority?}` -> `{findings: [SecurityFinding]}`. Without `status`: open, fixing and accepted.
    pub const SECURITY_FINDINGS: &str = "memory.security.findings";
    /// One finding with its history: `{project_id, id}` -> `SecurityFindingDetail`.
    pub const SECURITY_FINDING: &str = "memory.security.finding";
    /// A lifecycle move: `{project_id, id, status, note, by, session_id?, commit?}` -> the finding.
    /// Refused when the lifecycle does not allow the move (see [`super::SecurityFinding::status`]).
    pub const SECURITY_FINDING_UPDATE: &str = "memory.security.finding.update";
    /// Counts and the last audit: `{project_id}` -> `SecuritySummary`.
    pub const SECURITY_SUMMARY: &str = "memory.security.summary";
    /// Delete a finding and its history (the user's explicit delete): `{project_id, id}` -> `{}`.
    pub const SECURITY_FORGET: &str = "memory.security.forget";

    // Routines (the scheduler plugin's recurring and timed work, docs/SCHEDULES-PLAN.md):
    /// Create or replace a routine: [`super::Routine`] -> the routine as stored (its scope's store:
    /// a project id or `global`).
    pub const ROUTINE_PUT: &str = "memory.routine.put";
    /// One routine by id: `{id}` -> [`super::Routine`].
    pub const ROUTINE_GET: &str = "memory.routine.get";
    /// [`super::RoutineListParams`] -> [`super::RoutineListResult`]: a scope's routines (absent:
    /// every registered project's and the global ones).
    pub const ROUTINE_LIST: &str = "memory.routine.list";
    /// Remove a routine and its runs: `{id}` -> `{}`.
    pub const ROUTINE_REMOVE: &str = "memory.routine.remove";
    /// Create or replace a run of a routine: [`super::RoutineRun`] -> the run as stored.
    pub const ROUTINE_RUN_PUT: &str = "memory.routine.run.put";
    /// [`super::RoutineRunsParams`] -> [`super::RoutineRunsResult`]: a routine's runs, newest first.
    pub const ROUTINE_RUNS: &str = "memory.routine.runs";

    // The terminal's graph (docs/GRAPH-MODEL.md): every plugin's data, in the main project's
    // layers, as connected nodes and edges, plus streams and queues. CortexTerm's own methods.
    /// [`super::GraphPutParams`] -> [`super::GraphPutResult`]: merge nodes by `(label, key)` and
    /// edges by `(from, rel, to)`, in one batch. Every node and edge keeps `first_ms` and `last_ms`;
    /// values under `temporal` are added to that property's history. A node with no edge, in the
    /// batch or already stored, is refused (`Machine`, the root, excepted); so is an edge whose
    /// end is neither in the batch nor stored. The whole batch is checked before anything is
    /// written, so a refused batch writes nothing.
    pub const GRAPH_PUT: &str = "memory.graph.put";
    /// [`super::GraphRetireParams`] -> [`super::GraphRetireResult`]: mark nodes as gone
    /// (`ended_ms`, `state: 'gone'`); the node, its edges and its history stay. A later put of the
    /// node brings it back (both are cleared).
    pub const GRAPH_RETIRE: &str = "memory.graph.retire";
    /// [`super::GraphQueryParams`] -> [`super::GraphQueryResult`]: one read-only Cypher statement
    /// over the main project's three layers, with parameters, time-limited. A statement that could
    /// write, change the schema or name another project is refused.
    pub const GRAPH_QUERY: &str = "memory.graph.query";
    /// `{}` -> [`super::MachineInfo`]: this Mac's root node, `(:Machine {key, name, os})`, which
    /// the plugin creates at start; what other nodes link to.
    pub const GRAPH_MACHINE: &str = "memory.graph.machine";

    /// [`super::StreamAppendParams`] -> [`super::StreamAppendResult`]: append records to a topic
    /// (created on first use, with its retention), in order.
    pub const STREAM_APPEND: &str = "memory.stream.append";
    /// [`super::StreamReadParams`] -> [`super::StreamReadResult`]: a topic's records from an
    /// offset and/or a time, in order. A topic that does not exist reads as empty.
    pub const STREAM_READ: &str = "memory.stream.read";
    /// [`super::StreamTopicsParams`] -> [`super::StreamTopicsResult`]: the topics of the main
    /// project, with their record counts and retention.
    pub const STREAM_TOPICS: &str = "memory.stream.topics";

    /// [`super::QueuePublishParams`] -> [`super::QueuePublishResult`]: add messages to a work queue
    /// (created on first use).
    pub const QUEUE_PUBLISH: &str = "memory.queue.publish";
    /// [`super::QueueTakeParams`] -> [`super::QueueTakeResult`]: lease the next messages for a
    /// consumer. At least once: a message not acked within its lease is delivered again. Each
    /// consumer name reads the whole queue; workers sharing a name share its messages.
    pub const QUEUE_TAKE: &str = "memory.queue.take";
    /// [`super::QueueAckParams`] -> [`super::QueueAckResult`]: a consumer is done with messages;
    /// they are never delivered to it again.
    pub const QUEUE_ACK: &str = "memory.queue.ack";
}

pub mod topics {
    /// `{project_id, files, ms}` after an incremental or full index.
    pub const INDEXED: &str = "memory.indexed";
    pub const TASK_CHANGED: &str = "memory.task.changed";
    /// `{project_id, id?, status?}` after any write to the security findings or audits.
    pub const SECURITY_CHANGED: &str = "memory.security.changed";
    pub const BOARD_POSTED: &str = "memory.board.posted";
    /// `{path, value, by}` after a settings set or delete (`value` null when deleted).
    pub const SETTINGS_CHANGED: &str = "memory.settings.changed";
    /// An `EngineStatus` on every engine state change and, while the embedding model downloads,
    /// at most every 500 ms with the byte progress.
    pub const ENGINE: &str = "memory.engine";
    /// `{project_id, id, kind, state, action}` after a knowledge upsert, link, unlink, withdraw or
    /// supersede (`action` names which; `project_id` null for the global scope).
    pub const KNOWLEDGE_CHANGED: &str = "memory.knowledge.changed";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectOpenParams {
    pub root: String,
    /// The frame's project id (the plugin maps it to an IronGraph project). Empty: the id already
    /// registered for this root, else the id the frame gives the root (`p_` + 16 hex of SHA-256).
    #[serde(default)]
    pub project_id: String,
    /// Open another repository as a read-only reference (see [`methods::PROJECT_OPEN`]).
    #[serde(default)]
    pub reference: bool,
}

/// One known project in [`ProjectListResult`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectInfo {
    pub project_id: String,
    /// Short unique name: the folder name (characters other than `A-Z a-z 0-9 . _ -` become `-`),
    /// suffixed `-2`, `-3`, ... when another project has it. Stable once given.
    pub name: String,
    pub root: String,
    /// A read-only reference project (never written to).
    #[serde(default)]
    pub reference: bool,
    /// indexing | ready | error | closed (registered; opened by the first query)
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectListResult {
    /// By name.
    pub projects: Vec<ProjectInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectStatus {
    pub project_id: String,
    pub root: String,
    /// The project's short unique name ([`ProjectInfo::name`]).
    #[serde(default)]
    pub name: String,
    /// A read-only reference project.
    #[serde(default)]
    pub reference: bool,
    /// indexing | ready | error
    pub state: String,
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub described: u64,
    pub last_index_ms: u64,
    #[serde(default)]
    pub languages: Vec<String>,
    /// Packages the project's code imports from outside the repository (top-level names as the
    /// language writes them: `pandas`, `react`, `@scope/pkg`, `serde_json`, Go and Java import paths
    /// `database/sql`, `org.neo4j.driver`, C/C++ headers `sqlite3.h`) and the dependencies its
    /// manifests declare, sorted. What skills' `applies.imports` are matched against.
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// IronGraph's asynchronous embedding of the latest written symbol: `ready` once it is
    /// retrievable by semantic search, `pending` before.
    #[serde(default)]
    pub embedding: Option<String>,
    /// Public API of installed dependencies (offline docs layer): pending | ready.
    #[serde(default)]
    pub externals: Option<String>,
    #[serde(default)]
    pub external_symbols: u64,
    /// Per language: the language plugin (`lang@1`) that extracted its files.
    #[serde(default)]
    pub language_plugins: Vec<LanguagePlugin>,
    /// Files no language plugin covers (indexed as text or binary), by extension, most first.
    #[serde(default)]
    pub uncovered: Vec<UncoveredFiles>,
    /// Files a language plugin could not extract (an error, a timeout, not UTF-8), at most the
    /// first 100; [`ProjectStatus::extract_problem_count`] counts them all.
    #[serde(default)]
    pub extract_problems: Vec<ExtractProblem>,
    #[serde(default)]
    pub extract_problem_count: u64,
    /// What went wrong reaching the language plugins (listing them, `describe`), and language
    /// plugins that are not active.
    #[serde(default)]
    pub plugin_problems: Vec<String>,
    /// Progress of the cold index: files whose facts are in, of the files it covers
    /// (`total_files` is 0 until the tree is scanned). Equal once the index is done.
    #[serde(default)]
    pub indexed_files: u64,
    #[serde(default)]
    pub total_files: u64,
}

/// [`methods::PROJECT_CLOSE`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectCloseParams {
    /// The reference's project id, or its name ([`ProjectInfo::name`]).
    pub project_id: String,
}

/// The reference [`methods::PROJECT_CLOSE`] forgot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectCloseResult {
    pub project_id: String,
    pub name: String,
    pub root: String,
}

/// The language plugin that extracted a language's files.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LanguagePlugin {
    /// The language (`rust`, `tsx`, `cargo.toml`, ...).
    pub language: String,
    /// The plugin's category (`lang-rust`).
    pub plugin: String,
    pub version: String,
    pub files: u64,
}

/// Files of one extension no language plugin covers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UncoveredFiles {
    /// Without the dot; empty for files without one.
    pub extension: String,
    /// `text` (searchable by its leading text) | `binary`
    pub kind: String,
    pub files: u64,
    /// One of them (repository-relative).
    pub example: String,
}

/// A file a language plugin could not extract.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExtractProblem {
    pub path: String,
    /// The plugin's category; empty when no active plugin covers the file now.
    pub plugin: String,
    pub error: String,
    /// `text`: indexed as text, the error is the file node's `note`; `kept`: its content did not
    /// change since an earlier index extracted it, whose nodes the graph keeps until a plugin can
    /// extract it again.
    pub indexed_as: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceParams {
    pub project_id: String,
    /// Absolute path of the workspace directory.
    pub workspace: String,
}

/// Scope of a code query: the project, optionally seen through a workspace overlay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Scope {
    pub project_id: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymbolRef {
    /// Stable id (language-qualified path plus name).
    pub sid: String,
    /// function | method | class | struct | trait | interface | enum | type | const | field |
    /// variable | component | route | command | module | file | ...
    pub kind: String,
    pub name: String,
    pub qname: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default)]
    pub signature: String,
    /// What it does: written by `memory.describe`, else the index's summary (the words around it:
    /// doc, callers' comments, test names, docs, its own comments and messages), first line first.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default)]
    pub language: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchParams {
    #[serde(flatten)]
    pub scope: Scope,
    pub text: String,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub symbol: SymbolRef,
    pub score: f64,
    /// How it matched: semantic | text | name
    pub matched: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymbolParams {
    #[serde(flatten)]
    pub scope: Scope,
    /// A sid, a qualified name, or a bare name.
    pub name: String,
}

/// What the graph knows of a symbol. Its code is not here: it is read from the file on disk at
/// `symbol.path`, lines `start_line..=end_line`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymbolDetail {
    pub symbol: SymbolRef,
    #[serde(default)]
    pub params: Vec<ParamInfo>,
    #[serde(default)]
    pub returns: Option<String>,
    #[serde(default)]
    pub callers: u32,
    #[serde(default)]
    pub callees: u32,
    #[serde(default)]
    pub tests: Vec<SymbolRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParamInfo {
    pub name: String,
    pub position: u32,
    #[serde(default)]
    pub type_text: Option<String>,
    #[serde(default)]
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymbolMatches {
    /// All symbols the name resolved to (several when ambiguous).
    pub matches: Vec<SymbolDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgesParams {
    #[serde(flatten)]
    pub scope: Scope,
    pub sid: String,
    #[serde(default = "default_depth")]
    pub depth: u32,
}

fn default_depth() -> u32 {
    1
}

/// One call site: who calls, where, and with which argument expressions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallSite {
    pub caller: SymbolRef,
    pub callee: SymbolRef,
    pub path: String,
    pub line: u32,
    /// Argument expressions as written at the call site.
    #[serde(default)]
    pub args: Vec<String>,
    /// heuristic (structural resolution: import, receiver, same file or unique name),
    /// ambiguous (several same-named candidates), or resolved (language server)
    pub confidence: String,
    pub depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgesResult {
    pub sites: Vec<CallSite>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImpactParams {
    #[serde(flatten)]
    pub scope: Scope,
    /// Changed symbols (sids), or empty with `paths` to compute from changed files.
    #[serde(default)]
    pub sids: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "default_impact_depth")]
    pub depth: u32,
}

fn default_impact_depth() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImpactResult {
    pub changed: Vec<SymbolRef>,
    pub call_sites: Vec<CallSite>,
    pub tests: Vec<SymbolRef>,
    #[serde(default)]
    pub tasks: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathParams {
    #[serde(flatten)]
    pub scope: Scope,
    pub from: String,
    pub to: String,
    #[serde(default = "default_path_hops")]
    pub max_hops: u32,
}

fn default_path_hops() -> u32 {
    8
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathStep {
    pub symbol: SymbolRef,
    /// Relationship type leading to the next step (CALLS, IMPORTS, RENDERS, HANDLES, ...).
    #[serde(default)]
    pub via: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathResult {
    pub found: bool,
    pub steps: Vec<PathStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutlineParams {
    #[serde(flatten)]
    pub scope: Scope,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutlineResult {
    pub path: String,
    pub language: String,
    pub symbols: Vec<SymbolRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FocusParams {
    #[serde(flatten)]
    pub scope: Scope,
    pub path: String,
    /// What the reader looks for (a read's intent, a task statement).
    pub text: String,
}

/// One definition or section of the file and its match with the text (higher is closer).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FocusUnit {
    pub start_line: u32,
    pub end_line: u32,
    pub name: String,
    pub score: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FocusResult {
    /// Every scored unit of the file; empty when the file has none indexed or embedded.
    pub units: Vec<FocusUnit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoMapParams {
    #[serde(flatten)]
    pub scope: Scope,
    /// Files or sids the task is about (personalization seeds).
    #[serde(default)]
    pub focus: Vec<String>,
    pub budget_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoMapResult {
    /// Rendered map text (paths and signatures), within the token budget.
    pub text: String,
    pub files: u32,
    pub symbols: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilesParams {
    #[serde(flatten)]
    pub scope: Scope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FilesResult {
    /// Repository-relative paths, sorted.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DescribeParams {
    pub project_id: String,
    pub sid: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UndescribedParams {
    pub project_id: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

/// One machine-checkable acceptance check of a task contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContractCheck {
    pub name: String,
    /// command | manual
    pub kind: String,
    /// Shell command (kind=command) or reviewer note (manual).
    pub spec: String,
    /// Regex or substring the output must contain; exit status 0 is always required for commands.
    #[serde(default)]
    pub expect: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Contract {
    pub checks: Vec<ContractCheck>,
}

impl Contract {
    /// The contract's identity: SHA-256 of its JSON form (checks in order, every field).
    pub fn hash(&self) -> String {
        crate::sha256::hex(serde_json::to_string(self).unwrap_or_default().as_bytes())
    }
}

/// One accepted change of a frozen contract, kept on the task as its lineage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContractRevision {
    pub old_hash: String,
    pub new_hash: String,
    /// Why the acceptor accepted it.
    pub reason: String,
    /// What the proposal rested on (the failing checks and where they ran).
    #[serde(default)]
    pub evidence: String,
    /// The task's worker (agent name, or `session:<id>` for a foreground run).
    pub proposed_by: String,
    /// Never the author: the planner (`foreman`) or `user`.
    pub accepted_by: String,
    pub ts_ms: u64,
}

/// A request to change a frozen contract, carried by [`TaskUpdateParams::revision`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContractRevisionRequest {
    /// The hash the change replaces (must be the task's current one).
    pub old_hash: String,
    pub reason: String,
    #[serde(default)]
    pub evidence: String,
    pub proposed_by: String,
    pub accepted_by: String,
}

/// What a task update does to the task's contract (see [`contract_change`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContractChange {
    /// The contract to store, when it changes.
    pub contract: Option<Contract>,
    /// The hash to store, when it changes (freezing, or an accepted revision).
    pub hash: Option<String>,
    /// The lineage record to append.
    pub revision: Option<ContractRevision>,
}

/// The one rule for a task's contract, applied by every memory implementation:
/// - until frozen, the contract may be replaced freely; `freeze` stores the hash of the contract in
///   force (the given one, else the current one) and is a no-op on a frozen task;
/// - once frozen, a different contract is accepted only with a revision whose `old_hash` is the
///   current hash and whose acceptor is neither its proposer nor the task's assignee; the change is
///   appended to the lineage. The same contract again is a no-op.
pub fn contract_change(task: &Task, update: &TaskUpdateParams, now_ms: u64) -> Result<ContractChange, String> {
    let Some(frozen) = task.contract_hash.clone() else {
        let in_force = update.contract.as_ref().unwrap_or(&task.contract);
        return Ok(ContractChange { contract: update.contract.clone(), hash: update.freeze.then(|| in_force.hash()), revision: None });
    };
    let Some(contract) = update.contract.as_ref() else { return Ok(ContractChange::default()) };
    let new_hash = contract.hash();
    if new_hash == frozen {
        return Ok(ContractChange::default());
    }
    let Some(revision) = update.revision.as_ref() else {
        return Err(format!(
            "the contract of task {} is frozen ({frozen}); it changes only through a revision accepted by someone other than its author",
            task.id
        ));
    };
    if revision.old_hash != frozen {
        return Err(format!("the revision replaces contract {}, but task {} is at {frozen}", revision.old_hash, task.id));
    }
    let acceptor = revision.accepted_by.trim();
    let proposer = revision.proposed_by.trim();
    if acceptor.is_empty() || proposer.is_empty() || revision.reason.trim().is_empty() {
        return Err("a contract revision names its proposer, its acceptor and the reason".into());
    }
    if acceptor == proposer || task.assignee.as_deref().map(str::trim) == Some(acceptor) {
        return Err(format!("{acceptor} cannot accept a revision of a contract it works under"));
    }
    Ok(ContractChange {
        contract: Some(contract.clone()),
        hash: Some(new_hash.clone()),
        revision: Some(ContractRevision {
            old_hash: frozen,
            new_hash,
            reason: revision.reason.clone(),
            evidence: revision.evidence.clone(),
            proposed_by: proposer.to_owned(),
            accepted_by: acceptor.to_owned(),
            ts_ms: now_ms,
        }),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub title: String,
    /// Goal and context in detail.
    pub brief: String,
    /// The plan (steps) as written by the planner.
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub contract: Contract,
    /// pending | in_progress | verifying | done | failed | cancelled
    pub status: String,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Graph nodes (sids or paths) the task touches.
    #[serde(default)]
    pub touches: Vec<String>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Set when the task is claimed: from then on the contract changes only through an accepted
    /// revision (see [`contract_change`]).
    #[serde(default)]
    pub contract_hash: Option<String>,
    #[serde(default)]
    pub contract_revisions: Vec<ContractRevision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskCreateParams {
    pub project_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub title: String,
    pub brief: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub contract: Contract,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub touches: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskUpdateParams {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub contract: Option<Contract>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub touches: Option<Vec<String>>,
    /// Freeze the contract in force (on claim).
    #[serde(default)]
    pub freeze: bool,
    /// Required to change a frozen contract.
    #[serde(default)]
    pub revision: Option<ContractRevisionRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskGetParams {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskListParams {
    pub project_id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskListResult {
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardMessage {
    pub id: String,
    pub project_id: String,
    /// proposal | idea | claim | blocker | question | handoff | review_request | patch_offer | decision | status
    pub kind: String,
    /// Agent name, `user`, or `chat`.
    pub author: String,
    pub text: String,
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default)]
    pub to: Option<String>,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardPostParams {
    pub project_id: String,
    pub kind: String,
    pub author: String,
    pub text: String,
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardListParams {
    pub project_id: String,
    #[serde(default)]
    pub since_ms: Option<u64>,
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoardListResult {
    pub messages: Vec<BoardMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentRecord {
    pub name: String,
    pub project_id: String,
    /// worker | supervisor | reviewer | merger
    pub role: String,
    #[serde(default)]
    pub charter: String,
    /// idle | working | blocked | done | stopped
    pub status: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub tasks_done: u32,
    #[serde(default)]
    pub tasks_failed: u32,
    pub updated_ms: u64,
    /// The approval mode its runs use (`auto` | `ask`), as it was spawned with.
    #[serde(default = "default_agent_mode")]
    pub mode: String,
}

fn default_agent_mode() -> String {
    "auto".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentListParams {
    pub project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentListResult {
    pub agents: Vec<AgentRecord>,
}

/// A rule from CORTEX.md or learned (never code facts; those live in the graph).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub id: String,
    /// project id, or `global`
    pub scope: String,
    pub text: String,
    /// cortex_md | user | learned
    pub source: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleListParams {
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuleListResult {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyleEntry {
    pub id: String,
    /// communication | ui | backend | process | language:<lang>
    pub area: String,
    pub text: String,
    /// global or a project id
    pub scope: String,
    pub confidence: f32,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyleListParams {
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyleListResult {
    pub entries: Vec<StyleEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRecordResult {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinesParams {
    #[serde(flatten)]
    pub scope: Scope,
    /// Repository-relative path.
    pub path: String,
    /// 1-based inclusive range (default: the whole file).
    #[serde(default)]
    pub start: Option<u32>,
    #[serde(default)]
    pub end: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LineEntry {
    pub n: u32,
    pub tag: String,
    pub offset: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LinesResult {
    pub path: String,
    pub content_hash: String,
    pub total: u32,
    pub lines: Vec<LineEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRecordParams {
    pub project_id: String,
    pub session_id: String,
    #[serde(default)]
    pub task_id: Option<String>,
    /// done | failed | cancelled
    pub outcome: String,
    pub summary: String,
    #[serde(default)]
    pub touched: Vec<String>,
    #[serde(default)]
    pub metrics: Value,
}

/// One setting with its metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingValue {
    pub path: String,
    pub value: Value,
    /// JSON type: string | number | boolean | array | object
    #[serde(rename = "type")]
    pub value_type: String,
    #[serde(default)]
    pub description: Option<String>,
    pub updated_ms: u64,
    #[serde(default)]
    pub updated_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SettingsListParams {
    #[serde(default)]
    pub prefix: String,
    /// `global` (absent or empty) or a project id: whose settings.
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingsListResult {
    pub prefix: String,
    /// Nested JSON: sections are objects, settings are their values.
    pub settings: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingsDeleteParams {
    pub path: String,
    /// `global` (absent or empty) or a project id: whose settings.
    #[serde(default)]
    pub scope: String,
}

/// [`methods::JOURNAL_APPEND`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalAppendParams {
    pub session_id: String,
    pub patches: Vec<crate::contracts::frame::JournalPatch>,
}

/// [`methods::JOURNAL_APPEND`] result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalAppendResult {
    pub next: u64,
}

// ----- security findings -----

/// One audit run of a project by the security plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityAudit {
    pub id: String,
    pub started_ms: u64,
    #[serde(default)]
    pub finished_ms: Option<u64>,
    /// Units (attack classes) the audit had, and lines it judged.
    #[serde(default)]
    pub units: u32,
    #[serde(default)]
    pub lines_judged: u32,
    /// Confirmed findings, leads and hardening notes it produced.
    #[serde(default)]
    pub findings: u32,
    #[serde(default)]
    pub leads: u32,
    #[serde(default)]
    pub hardening: u32,
    /// The scope the audit was limited to (a path), when any.
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub skill_version: String,
    /// Where the Markdown export of the report was written, when it was.
    #[serde(default)]
    pub report_path: Option<String>,
}

/// One step of a finding's trace, entry point first, sink last.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityTraceStep {
    /// `entrypoint`, `step` or `sink`.
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub what: String,
}

/// A fix the host can spell out exactly (today: a dependency upgrade).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityQuickFix {
    /// `upgrade`.
    pub kind: String,
    pub ecosystem: String,
    pub package: String,
    pub from: String,
    pub to: String,
    pub lockfile: String,
    /// The commands that make the change, in order.
    #[serde(default)]
    pub commands: Vec<String>,
}

/// One security finding of a project: a confirmed vulnerability, a lead (one fact outside the code
/// decides) or a hardening note. Identified by its fingerprint (`<class>:<file>:<function>`).
///
/// `status` lifecycle, moves the store allows: `open` -> `accepted` | `fixing` | `gone`, and
/// `fixed` by a re-check; `fixing` -> `fixed` | `open`; `accepted` -> `open` | `gone`;
/// `fixed` -> `verified` | `open`; `verified` -> `open`; `gone` -> `open`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityFinding {
    pub id: String,
    pub class: String,
    /// `confirmed`, `needs_validation` or `hardening`.
    pub verdict: String,
    pub title: String,
    #[serde(default)]
    pub principal: String,
    #[serde(default)]
    pub control: String,
    #[serde(default)]
    pub impact: String,
    /// The demonstrated result, one line.
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub evidence_how: String,
    #[serde(default)]
    pub evidence_observed: String,
    #[serde(default)]
    pub severity_likelihood: Option<String>,
    #[serde(default)]
    pub severity_impact: Option<String>,
    /// `critical`, `high`, `medium` or `low` for confirmed findings.
    #[serde(default)]
    pub severity_overall: Option<String>,
    #[serde(default)]
    pub fix_where: String,
    #[serde(default)]
    pub fix_change: String,
    #[serde(default)]
    pub fix_test: String,
    /// For a lead: the fact outside the code that decides it.
    #[serde(default)]
    pub missing_fact: Option<String>,
    #[serde(default)]
    pub trace: Vec<SecurityTraceStep>,
    #[serde(default)]
    pub quick_fix: Option<SecurityQuickFix>,
    /// Computed by the writer: higher first.
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub priority_reason: String,
    // Lifecycle, owned by the store (ignored on put for a finding already known):
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub status_note: String,
    /// `user`, `model` or `recheck`.
    #[serde(default)]
    pub status_by: String,
    #[serde(default)]
    pub status_ms: u64,
    #[serde(default)]
    pub first_seen_ms: u64,
    #[serde(default)]
    pub last_seen_ms: u64,
    /// How many audits reported it.
    #[serde(default)]
    pub seen_in: u32,
    #[serde(default)]
    pub audit_id: Option<String>,
    /// The session that fixes or fixed it, and the commit that holds the fix.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub fixed_commit: Option<String>,
    /// The sink's place (the trace's last step), for the list.
    #[serde(default)]
    pub sink_path: Option<String>,
    #[serde(default)]
    pub sink_line: Option<u32>,
}

/// One lifecycle move of a finding.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityStatusChange {
    pub status: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub by: String,
    pub ms: u64,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
}

/// [`methods::SECURITY_AUDIT_PUT`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecurityAuditPutParams {
    pub project_id: String,
    pub audit: SecurityAudit,
}

/// [`methods::SECURITY_FINDINGS_PUT`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecurityFindingsPutParams {
    pub project_id: String,
    #[serde(default)]
    pub audit_id: Option<String>,
    pub findings: Vec<SecurityFinding>,
}

/// [`methods::SECURITY_FINDINGS`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityFindingsParams {
    pub project_id: String,
    /// Statuses to list (absent: open, fixing and accepted).
    #[serde(default)]
    pub status: Option<Vec<String>>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub min_priority: Option<i64>,
}

/// [`methods::SECURITY_FINDINGS`] and [`methods::SECURITY_FINDINGS_PUT`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityFindingsResult {
    pub findings: Vec<SecurityFinding>,
}

/// [`methods::SECURITY_FINDING`] and [`methods::SECURITY_FORGET`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecurityFindingParams {
    pub project_id: String,
    pub id: String,
}

/// [`methods::SECURITY_FINDING`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityFindingDetail {
    pub finding: SecurityFinding,
    #[serde(default)]
    pub history: Vec<SecurityStatusChange>,
}

/// [`methods::SECURITY_FINDING_UPDATE`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecurityFindingUpdateParams {
    pub project_id: String,
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub note: String,
    /// `user`, `model` or `recheck`.
    pub by: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
}

/// [`methods::SECURITY_SUMMARY`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecuritySummaryParams {
    pub project_id: String,
}

/// [`methods::SECURITY_SUMMARY`] result: the active list's shape and the last audit.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecuritySummary {
    #[serde(default)]
    pub last_audit: Option<SecurityAudit>,
    #[serde(default)]
    pub open: u32,
    #[serde(default)]
    pub fixing: u32,
    #[serde(default)]
    pub accepted: u32,
    #[serde(default)]
    pub fixed: u32,
    #[serde(default)]
    pub verified: u32,
    /// Open and fixing findings by overall severity (`critical`, `high`, `medium`, `low`,
    /// `lead`, `hardening`).
    #[serde(default)]
    pub by_severity: std::collections::BTreeMap<String, u32>,
}

/// [`topics::SECURITY_CHANGED`] payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SecurityChanged {
    pub project_id: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// [`methods::NOTE_ADD`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteAddParams {
    /// Absent or empty: a global note.
    #[serde(default)]
    pub project_id: Option<String>,
    pub topic: String,
    /// The finding, in the note writer's words.
    pub claim: String,
    /// Verbatim text from the source that supports the claim.
    pub quote: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// The source's date (`YYYY-MM-DD`).
    #[serde(default)]
    pub date: Option<String>,
    /// Where it was found (`arxiv`, `web`, ...).
    #[serde(default)]
    pub source: Option<String>,
    /// The question it answers.
    #[serde(default)]
    pub question: Option<String>,
    /// Who wrote it: `user`, `model`, a job id.
    #[serde(default)]
    pub by: Option<String>,
}

/// A cited note: a `Note` node in the KNOWLEDGE layer that `CITES` a `Source {url}` node.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Note {
    /// `n-` + 16 hex of SHA-256 of scope, topic key, url and quote.
    pub id: String,
    /// None: a global note.
    #[serde(default)]
    pub project_id: Option<String>,
    pub topic: String,
    pub claim: String,
    pub quote: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
    pub created_ms: u64,
    /// Similarity to the search text (IronGraph's embedding score) in a search result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NoteSearchParams {
    /// Absent or empty: the global notes.
    #[serde(default)]
    pub project_id: Option<String>,
    pub text: String,
    /// Only this topic's notes (topics compare case- and punctuation-insensitively).
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteSearchResult {
    pub notes: Vec<Note>,
    /// Notes in scope that IronGraph has not embedded yet (it embeds asynchronously, in write
    /// order): not ranked by this search.
    #[serde(default)]
    pub pending: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteListParams {
    /// Absent or empty: the global notes.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Absent: every topic.
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteListResult {
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteRemoveParams {
    /// Absent or empty: a global note.
    #[serde(default)]
    pub project_id: Option<String>,
    pub id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteRemoveResult {
    /// False when no such note existed.
    pub removed: bool,
}

// ---- project knowledge -----------------------------------------------------------------------

/// The kinds of a [`KnowledgeItem`] and the states each kind may be in.
pub mod knowledge {
    pub const KINDS: &[&str] = &[DESCRIPTION, CONCEPT, FACT, DECISION, HYPOTHESIS, QUESTION, LESSON];
    /// What the software is, in a short paragraph without code (what it does, its core features,
    /// how it is built, run and tested): one per project, written by the model from the
    /// repository's own documents, shown first in the brief.
    pub const DESCRIPTION: &str = "description";
    /// Domain vocabulary: what a word means here, bound to the code that implements it.
    pub const CONCEPT: &str = "concept";
    /// A claim about the project's world, with a source and an `as_of` date.
    pub const FACT: &str = "fact";
    /// What was decided and why (the rejected alternatives in `text` or `evidence`).
    pub const DECISION: &str = "decision";
    /// A working or refuted theory, with the runs that tested it.
    pub const HYPOTHESIS: &str = "hypothesis";
    /// A known unknown.
    pub const QUESTION: &str = "question";
    /// How to work, learned from what happened in a run (a tool's behaviour, a language's trap,
    /// an approach that worked or failed): one sentence, kept in the global scope so every project
    /// gets it; its name starts with its anchor (`tool:edit: ...`, `lang:rust: ...`, `approach: ...`).
    pub const LESSON: &str = "lesson";

    /// States shared by every kind.
    pub const WITHDRAWN: &str = "withdrawn";
    pub const SUPERSEDED: &str = "superseded";
    /// Concept and fact: `asserted`; decision: `accepted`; hypothesis: `open | supported |
    /// refuted`; question: `open | answered`.
    pub fn states(kind: &str) -> &'static [&'static str] {
        match kind {
            DESCRIPTION | CONCEPT | FACT | LESSON => &["asserted", SUPERSEDED, WITHDRAWN],
            DECISION => &["accepted", SUPERSEDED, WITHDRAWN],
            HYPOTHESIS => &["open", "supported", "refuted", SUPERSEDED, WITHDRAWN],
            QUESTION => &["open", "answered", SUPERSEDED, WITHDRAWN],
            _ => &[],
        }
    }
    /// A kind's state when none is given.
    pub fn default_state(kind: &str) -> &'static str {
        match kind {
            DECISION => "accepted",
            HYPOTHESIS | QUESTION => "open",
            _ => "asserted",
        }
    }
    /// A state in which an item is served (search, `linked`, the brief).
    pub fn is_live(state: &str) -> bool {
        state != WITHDRAWN && state != SUPERSEDED
    }

    /// Who wrote an item.
    pub const SOURCES: &[&str] = &["user", "agent", "research", "repo_doc", "inferred"];

    /// The relations an item may have, with what each targets: `implemented_by` and `about` a code
    /// reference (sid, repository path, qualified or unique bare name); `described_in` a document
    /// section's sid; `supported_by` a note id; `tested_by` a run id; `answers` another item's id.
    pub const RELATIONS: &[&str] = &["implemented_by", "about", "described_in", "supported_by", "tested_by", "answers"];
}

/// One link of a [`KnowledgeItem`]: `rel` is one of [`knowledge::RELATIONS`]; `target` is what was
/// given to link (a code reference, a note id, a run id, an item id) and, once stored, `id` is the
/// resolved node's identity (a code sid, the note's id, ...) and `name` its display name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeLink {
    pub rel: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// A knowledge item: a `Knowledge` node (also labelled by its kind) in the KNOWLEDGE layer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeItem {
    /// `k-` + 16 hex of SHA-256 of kind, scope and the normalized name.
    pub id: String,
    /// None: the global scope.
    #[serde(default)]
    pub project_id: Option<String>,
    pub kind: String,
    /// Short: what the item is about (a concept's term, a fact's subject, a hypothesis in a phrase).
    pub name: String,
    /// One or two sentences.
    pub text: String,
    pub state: String,
    /// One of [`knowledge::SOURCES`].
    pub source: String,
    pub confidence: f32,
    /// When the claim held (`YYYY-MM-DD`); the creation date when absent.
    #[serde(default)]
    pub as_of: Option<String>,
    /// Who wrote it: `user`, a session id, an agent name, a job id.
    #[serde(default)]
    pub by: String,
    /// URIs: `journal://<session>/<seq>`, `run://<id>`, `note://<id>`, `commit://<sha>`, a url.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Other names (a glossary: `hdcp` for handicap); exact matches in search.
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub links: Vec<KnowledgeLink>,
    /// How often the item was learned again, and how it fared where it was shown: the error it
    /// is about did not come back in a run that went well (`helped`), or came back (`hurt`).
    #[serde(default)]
    pub seen: u32,
    #[serde(default)]
    pub helped: u32,
    #[serde(default)]
    pub hurt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Why it was withdrawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Similarity to the search text in a search result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// [`methods::KNOWLEDGE_UPSERT`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeUpsertParams {
    /// Absent or empty: the global scope.
    #[serde(default)]
    pub project_id: Option<String>,
    pub kind: String,
    pub name: String,
    pub text: String,
    /// Absent: the kind's default state for a new item, unchanged for an existing one.
    #[serde(default)]
    pub state: Option<String>,
    /// Absent: `user`.
    #[serde(default)]
    pub source: Option<String>,
    /// Absent: 1.0 for `user`, 0.5 otherwise, unchanged for an existing item.
    #[serde(default)]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub as_of: Option<String>,
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Links to add; a target that does not resolve is reported in the item's `links` with an
    /// empty `id` and is not stored.
    #[serde(default)]
    pub links: Vec<KnowledgeLink>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeGetParams {
    #[serde(default)]
    pub project_id: Option<String>,
    pub id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeListParams {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// Also the withdrawn and superseded items.
    #[serde(default)]
    pub include_history: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeListResult {
    pub items: Vec<KnowledgeItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeSearchParams {
    #[serde(default)]
    pub project_id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "default_knowledge_limit")]
    pub limit: u32,
}

fn default_knowledge_limit() -> u32 {
    5
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeSearchResult {
    pub items: Vec<KnowledgeItem>,
    /// Live items in scope IronGraph has not embedded yet: not ranked by this search.
    #[serde(default)]
    pub pending: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeLinkParams {
    #[serde(default)]
    pub project_id: Option<String>,
    pub id: String,
    pub rel: String,
    pub target: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeWithdrawParams {
    #[serde(default)]
    pub project_id: Option<String>,
    pub id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub by: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeSupersedeParams {
    #[serde(default)]
    pub project_id: Option<String>,
    /// The item that is superseded.
    pub id: String,
    /// The item that supersedes it.
    pub by: String,
}

/// [`methods::KNOWLEDGE_COUNT`] parameters: what to add to each counter.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeCountParams {
    #[serde(default)]
    pub project_id: Option<String>,
    pub id: String,
    #[serde(default)]
    pub seen: u32,
    #[serde(default)]
    pub helped: u32,
    #[serde(default)]
    pub hurt: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeLinkedParams {
    #[serde(default)]
    pub project_id: Option<String>,
    /// Code sids (symbols, `file:<path>` files, document sections).
    #[serde(default)]
    pub sids: Vec<String>,
    /// Repository paths: items bound to the file or to any symbol in it.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// One item bound to served code.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeLinked {
    /// The code sid the item is bound to.
    pub sid: String,
    pub rel: String,
    pub item: KnowledgeItem,
    /// `<kind>[ <state>] (<source>, <date>): <text>  knowledge://<id>`, what the host appends to
    /// the code it serves.
    pub line: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeLinkedResult {
    /// By sid in the order asked, then by item kind and name.
    pub items: Vec<KnowledgeLinked>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeBriefParams {
    #[serde(default)]
    pub project_id: Option<String>,
    /// Tokens as the model counts them (`model.count`; bytes when no model answers). 0: nothing.
    pub budget_tokens: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeBriefResult {
    /// One line per item, grouped (decisions, facts, concepts, open questions); empty when there
    /// is nothing live or the budget is 0.
    pub text: String,
    /// The ids the text holds, in order.
    pub ids: Vec<String>,
    pub tokens: u32,
    /// Whether a model counted (false: bytes).
    pub by_model: bool,
}

/// The memory plugin's IronGraph engine: where its embedding model and cache live (always under
/// `CORTEX_HOME`), how the pinned model got there, and installation progress.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EngineStatus {
    /// installing | loading | ready | error
    pub state: String,
    /// Embedding model repository and pinned revision.
    pub model_id: String,
    pub revision: String,
    pub model_dir: String,
    pub cache_dir: String,
    /// Model bytes present and verified or downloaded so far, of `bytes_total` (all model files).
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Bytes this process fetched over the network, and the partial bytes a resumed download
    /// started from (0 for a fresh download).
    #[serde(default)]
    pub downloaded_bytes: u64,
    #[serde(default)]
    pub resumed_from: u64,
    /// How the model files arrived on this start: `installed` (already present and verified),
    /// `local:<dir>` (copied from another verified installation) or `network`.
    #[serde(default)]
    pub source: Option<String>,
    /// Time spent installing / verifying the model, and opening the database (model load + warm-up).
    #[serde(default)]
    pub install_ms: Option<u64>,
    #[serde(default)]
    pub open_ms: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A routine: recurring or timed work (docs/SCHEDULES-PLAN.md). The scheduler plugin owns its
/// meaning; memory keeps it in its scope's store (`scope`: a project id or `global`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Routine {
    pub id: String,
    pub scope: String,
    pub title: String,
    /// wake | handoff | command
    pub kind: String,
    /// What to do, in the creator's words (wake and handoff).
    #[serde(default)]
    pub brief: String,
    /// The command line (command).
    #[serde(default)]
    pub command: String,
    /// The schedule as written (`every 5m`, `every mon at 09:00`).
    pub when: String,
    /// The schedule as the scheduler reads it (its own JSON).
    #[serde(default)]
    pub schedule: Value,
    #[serde(default)]
    pub next_ms: Option<u64>,
    #[serde(default)]
    pub last_due_ms: Option<u64>,
    /// active | paused | ended
    pub state: String,
    #[serde(default)]
    pub state_reason: String,
    /// The session a wake goes to.
    #[serde(default)]
    pub report_to: Option<String>,
    /// The session whose children a handoff or command routine's runs are.
    #[serde(default)]
    pub home_session: Option<String>,
    /// Where it was made: the session and the model run.
    #[serde(default)]
    pub created_session: Option<String>,
    #[serde(default)]
    pub created_run: Option<String>,
    /// Task ids the routine exists for: when all are finished, it ends.
    #[serde(default)]
    pub watches: Vec<String>,
    /// A wake that is an agent check-in (the session decides in MANAGE).
    #[serde(default)]
    pub manage: bool,
    #[serde(default)]
    pub until_ms: Option<u64>,
    #[serde(default)]
    pub max_runs: Option<u32>,
    #[serde(default)]
    pub run_count: u32,
    #[serde(default)]
    pub fail_streak: u32,
    #[serde(default)]
    pub last_outcome: String,
    #[serde(default)]
    pub last_summary: String,
    #[serde(default)]
    pub last_ok_ms: Option<u64>,
    /// The approval mode its runs use (`auto` | `ask`).
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub flow: Option<String>,
    /// model | user | agents
    #[serde(default)]
    pub by: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// One firing of a routine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RoutineRun {
    pub id: String,
    pub routine_id: String,
    pub due_ms: u64,
    pub started_ms: u64,
    #[serde(default)]
    pub ended_ms: Option<u64>,
    /// running | done | failed | skipped | woke
    pub outcome: String,
    #[serde(default)]
    pub summary: String,
    /// The session the run worked in (a handoff's agent session, the woken session).
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RoutineListParams {
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RoutineListResult {
    pub routines: Vec<Routine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutineRunsParams {
    pub id: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RoutineRunsResult {
    pub runs: Vec<RoutineRun>,
}

// ---- The terminal's graph: nodes and edges, streams, queues ----

/// The layer of the main project a [`GraphPutParams`] batch creates its nodes and edges in. A
/// node keeps the layer it was created in; later puts from any layer update it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GraphLayer {
    /// What CortexTerm saw: Runs, apps, packages, connections, devices, findings.
    #[default]
    Observed,
    /// Curated facts: habits, lessons, advisories that affect this Mac. Each node needs a `name`
    /// (its `key` when the props give none).
    Knowledge,
    /// The app's own records: projects, sessions, settings, queue offsets.
    Workspace,
}

/// A node by identity: its label and key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GraphRef {
    /// CamelCase: an ASCII capital, then letters and digits (`App`, `StartItem`).
    pub label: String,
    /// Unique within the label (a bundle id, a path, `ecosystem:name`…). At most 1024 bytes.
    pub key: String,
}

/// One node of a [`GraphPutParams`] batch.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphNode {
    pub label: String,
    pub key: String,
    /// Ordinary properties, replaced by each put (a property not named is left as it is; `null`
    /// removes one). Names are snake_case (`[a-z][a-z0-9_]*`); values are strings, numbers,
    /// booleans, or lists of one scalar type. Maps are refused: structure is modelled as nodes and
    /// edges, never as JSON text. `key`, `first_ms`, `last_ms`, `ended_ms`, `ended_reason` and
    /// `created_at` are the plugin's.
    #[serde(default)]
    pub props: serde_json::Map<String, Value>,
    /// Temporal properties: a value that differs from the current one is added to the
    /// property's history at the batch's `at_ms` (so the history holds the changes, and repeating
    /// a put adds nothing); the latest sample is the current value. A string, number or boolean, the same type every time
    /// for one label's property (an integer is taken for a float property). A property is
    /// temporal from its first use here: name it here from the first write, never under `props`
    /// first.
    #[serde(default)]
    pub temporal: serde_json::Map<String, Value>,
}

/// One edge of a [`GraphPutParams`] batch, merged by `(from, rel, to)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphEdge {
    pub from: GraphRef,
    /// UPPER_SNAKE: an ASCII capital, then capitals, digits and `_` (`INSTALLED_ON`).
    pub rel: String,
    pub to: GraphRef,
    /// As [`GraphNode::props`].
    #[serde(default)]
    pub props: serde_json::Map<String, Value>,
    /// As [`GraphNode::temporal`], per relationship type.
    #[serde(default)]
    pub temporal: serde_json::Map<String, Value>,
}

/// [`methods::GRAPH_PUT`]: at most 5000 nodes and edges together per call.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphPutParams {
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
    /// Where new nodes and edges are created; `observed` when absent.
    #[serde(default)]
    pub layer: GraphLayer,
    /// When this was seen (epoch ms): the time of the temporal samples and of `first_ms` /
    /// `last_ms`. Now when absent; never more than a minute ahead.
    #[serde(default)]
    pub at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphPutResult {
    /// Distinct nodes written.
    pub nodes: u32,
    /// Distinct edges written.
    pub edges: u32,
}

/// [`methods::GRAPH_RETIRE`]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphRetireParams {
    pub refs: Vec<GraphRef>,
    /// Why (`uninstalled`, `fixed`…), kept as `ended_reason`.
    #[serde(default)]
    pub reason: Option<String>,
    /// When it went (epoch ms); now when absent.
    #[serde(default)]
    pub at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphRetireResult {
    /// Nodes found and not already retired.
    pub retired: u32,
}

/// [`methods::GRAPH_QUERY`]: one statement, without `USE` (the plugin names the project and
/// layers). A point-in-time read starts with `AT TIME <datetime>`; histories are read with
/// `HISTORY n.prop FROM <datetime> TO <datetime> AS s` (`s.time` in epoch nanoseconds, `s.value`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphQueryParams {
    pub cypher: String,
    /// `$name` parameters: values are never written into the statement.
    #[serde(default)]
    pub params: serde_json::Map<String, Value>,
    /// Rows returned at most: 200 when absent, 5000 at most.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphQueryResult {
    pub columns: Vec<String>,
    /// Plain JSON values; a node is `{id, labels, properties}`, a relationship `{id, type, start,
    /// end, properties}`.
    pub rows: Vec<Vec<Value>>,
    /// More rows matched than `limit`.
    #[serde(default)]
    pub truncated: bool,
}

/// [`methods::GRAPH_MACHINE`]: this Mac's root node.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MachineInfo {
    /// The hardware UUID (`IOPlatformUUID`).
    pub key: String,
    /// The computer name.
    pub name: String,
    /// `macOS 26.1 (25B78)`.
    pub os: String,
}

/// [`methods::STREAM_APPEND`]. Topic names: `[a-z0-9][a-z0-9._-]*`, at most 200 bytes; `queue.` and
/// `session.` topics belong to the plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamAppendParams {
    pub topic: String,
    /// JSON values, in order; at most 5000 per call and 1 MB each.
    pub records: Vec<Value>,
    /// How long records are kept, applied when the topic is created: 30 days when absent, 1 to 3650.
    #[serde(default)]
    pub retention_days: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamAppendResult {
    /// The offset of the first record appended.
    pub first_offset: u64,
    pub count: u32,
}

/// [`methods::STREAM_READ`]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamReadParams {
    pub topic: String,
    /// The first offset to read (the last read's `next_offset`); the oldest kept when absent.
    #[serde(default)]
    pub from_offset: Option<u64>,
    /// Only records appended at or after this time (epoch ms).
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// Records returned at most: 500 when absent, 5000 at most.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// One record of a topic.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamEntry {
    pub offset: u64,
    /// When it was appended (epoch ms).
    pub at_ms: u64,
    pub value: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamReadResult {
    pub records: Vec<StreamEntry>,
    /// Where the next read starts.
    pub next_offset: u64,
    /// Nothing follows (yet).
    pub end: bool,
}

/// [`methods::STREAM_TOPICS`]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamTopicsParams {
    /// Only topics whose name starts with this.
    #[serde(default)]
    pub prefix: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamTopic {
    pub name: String,
    /// Records kept now.
    pub records: u64,
    /// The offset after the last record.
    pub next_offset: u64,
    /// None: kept for good.
    #[serde(default)]
    pub retention_days: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamTopicsResult {
    pub topics: Vec<StreamTopic>,
}

/// [`methods::QUEUE_PUBLISH`]. Queue names: as topic names, at most 190 bytes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueuePublishParams {
    pub queue: String,
    /// JSON values, in order; at most 5000 per call and 1 MB each.
    pub messages: Vec<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueuePublishResult {
    pub count: u32,
}

/// [`methods::QUEUE_TAKE`]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueTakeParams {
    pub queue: String,
    /// Who reads: `[A-Za-z0-9._-]`, at most 100 bytes (a plugin's name, `notify`).
    pub consumer: String,
    /// Messages at most: 10 when absent, 1000 at most.
    #[serde(default)]
    pub max: Option<u32>,
    /// How long the consumer has to ack before a message is delivered again: 60 s when absent,
    /// 1 s to 24 h.
    #[serde(default)]
    pub lease_ms: Option<u64>,
}

/// A message handed to a consumer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueMessage {
    /// What [`methods::QUEUE_ACK`] takes.
    pub id: u64,
    pub value: Value,
    /// When it was published (epoch ms).
    pub at_ms: u64,
    /// 1 on the first delivery, more when it is delivered again.
    pub attempt: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueTakeResult {
    /// Oldest first: redeliveries of expired leases, then new messages. Empty when none wait.
    pub messages: Vec<QueueMessage>,
}

/// [`methods::QUEUE_ACK`]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueAckParams {
    pub queue: String,
    pub consumer: String,
    pub ids: Vec<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueAckResult {
    /// Messages that were waiting for this ack (acking one twice counts once).
    pub acked: u32,
}
