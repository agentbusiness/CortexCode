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

pub mod methods {
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

    /// Settings in the main project, as a node tree (Settings -> SettingsSection -> Setting).
    /// Paths are dotted: sections, then the key (`model.variant`, `plugins.memory.depth`).
    pub const SETTINGS_GET: &str = "memory.settings.get";
    /// Create or update one setting; sections are created on demand. A value's JSON type is fixed
    /// once set (changing it requires `memory.settings.delete` first).
    pub const SETTINGS_SET: &str = "memory.settings.set";
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

    /// The app's projects (the frame's records), from the main store: `{projects: [Project]}`.
    pub const APP_PROJECTS: &str = "memory.app.projects";
    /// Create or replace one project record: `{project: Project}`.
    pub const APP_PROJECT_PUT: &str = "memory.app.project.put";
    /// Delete one project record and its settings: `{id}`. Its sessions are deleted with
    /// [`APP_SESSIONS_DELETE`] first.
    pub const APP_PROJECT_DELETE: &str = "memory.app.project.delete";
    /// The app's sessions (chats and agent runs), each linked to its project: `{sessions: [Session]}`.
    pub const APP_SESSIONS: &str = "memory.app.sessions";
    /// Create or replace one session record: `{session: Session}`.
    pub const APP_SESSION_PUT: &str = "memory.app.session.put";
    /// Delete session records and their journals: `{ids: [id]}`.
    pub const APP_SESSIONS_DELETE: &str = "memory.app.sessions.delete";
    /// Append a session's journal patches, in order, to its stream: `{session_id, patches:
    /// [JournalPatch]}` -> `{next}` (the offset after the last one). Frames before the move of
    /// journals into files wrote here; the frame no longer calls it.
    pub const JOURNAL_APPEND: &str = "memory.journal.append";
    /// A session's journal patches from an offset: `{session_id, from, max}` -> `{patches, next,
    /// end}` (`end`: nothing after `next`). A session without a journal reads as empty. The frame
    /// reads a stream once, to move it into the session's journal file.
    pub const JOURNAL_READ: &str = "memory.journal.read";
    /// Drop a session's journal stream (after its file is written): `{session_id}` -> `{}`. A
    /// session without a stream is fine.
    pub const JOURNAL_DROP: &str = "memory.journal.drop";

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingsGetParams {
    pub path: String,
    /// `global` (absent or empty) or a project id: whose settings.
    #[serde(default)]
    pub scope: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SettingsSetParams {
    pub path: String,
    pub value: Value,
    /// `global` (absent or empty) or a project id: whose settings.
    #[serde(default)]
    pub scope: String,
    /// Who changed it (`user`, a plugin id, an agent name).
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub description: Option<String>,
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

/// [`methods::APP_PROJECT_PUT`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppProjectPutParams {
    pub project: crate::contracts::frame::Project,
}

/// [`methods::APP_PROJECTS`] result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppProjectsResult {
    pub projects: Vec<crate::contracts::frame::Project>,
}

/// [`methods::APP_PROJECT_DELETE`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppProjectDeleteParams {
    pub id: String,
}

/// [`methods::APP_SESSION_PUT`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSessionPutParams {
    pub session: crate::contracts::frame::Session,
}

/// [`methods::APP_SESSIONS`] result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppSessionsResult {
    pub sessions: Vec<crate::contracts::frame::Session>,
}

/// [`methods::APP_SESSIONS_DELETE`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSessionsDeleteParams {
    pub ids: Vec<String>,
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

/// [`methods::JOURNAL_READ`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalReadParams {
    pub session_id: String,
    #[serde(default)]
    pub from: u64,
    /// At most this many patches (absent: the plugin's page size).
    #[serde(default)]
    pub max: Option<usize>,
}

/// [`methods::JOURNAL_READ`] result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalReadResult {
    pub patches: Vec<crate::contracts::frame::JournalPatch>,
    pub next: u64,
    pub end: bool,
}

/// [`methods::JOURNAL_DROP`] parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalDropParams {
    pub session_id: String,
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
