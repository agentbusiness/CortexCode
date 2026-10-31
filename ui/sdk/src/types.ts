/**
 * TypeScript mirrors of the CPP/1 contracts the workbench uses (`protocol/src/contracts/*.rs`).
 *
 * Field names and optionality follow the Rust structs exactly: a Rust `Option<T>` with
 * `#[serde(default)]` is `T | null | undefined` here, because serde writes `null` for `None`.
 */

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

// ─── frame ──────────────────────────────────────────────────────────────────

export interface Project {
  id: string;
  root: string;
  name: string;
  created_ms: number;
  /** The flows (ways of working) that work in it: `coding`, later `office`, ... */
  flows?: string[];
}

export type SessionKind = 'chat' | 'agent';
export type SessionStatus = 'idle' | 'running' | 'waiting' | 'done' | 'failed' | 'cancelled';

export interface Session {
  id: string;
  project_id: string;
  kind: SessionKind;
  title: string;
  agent?: string | null;
  parent?: string | null;
  workspace: string;
  task_id?: string | null;
  status: SessionStatus;
  created_ms: number;
  updated_ms: number;
  /** The flow the session belongs to, for life (`coding`); its calls go to `chatflow-<flow>`. */
  flow?: string;
}

/** A session's flow: its calls go to `chatflow-<flow>.<verb>`. Records from before flows: coding. */
export function flowMethod(flow: string | undefined | null, verb: string): string {
  return `chatflow-${flow || 'coding'}.${verb}`;
}

export interface JournalEntry {
  seq: number;
  ts_ms: number;
  session_id: string;
  kind: string;
  data: { [key: string]: Json };
}

export type JournalPatch =
  | { op: 'create'; entry: JournalEntry; rev?: number }
  | { op: 'append'; session_id: string; seq: number; field: string; text: string; rev?: number }
  | { op: 'set'; session_id: string; seq: number; field: string; value: Json; rev?: number };

export interface UiPlugin {
  id: string;
  version: string;
  slot: string;
  entry_url: string;
  css_url?: string | null;
  hash: string;
  /** From the plugin's manifest.json (read by the workbench): how a host names it (a tab, a
   * section, a settings panel), and where it sorts among its slot family (lower first). */
  label?: string;
  order?: number;
  /** From the manifest: the flow whose sessions it shows (`coding`), when it belongs to one. */
  flow?: string;
}

export interface PluginInfo {
  plugin_id: string;
  version: string;
  category: string;
  contract: string;
  instance: string;
  state: string;
  pid: number;
  exec: string;
  started_ms: number;
  restarts?: number;
}

/** Journal entry kinds (`protocol/src/contracts/frame.rs`, `kinds`). */
export const kinds = {
  USER_MESSAGE: 'user_message',
  ASSISTANT_MESSAGE: 'assistant_message',
  REASONING: 'reasoning',
  TOOL_CALL: 'tool_call',
  TOOL_RESULT: 'tool_result',
  STEER: 'steer',
  ROUTE: 'route',
  STATUS: 'status',
  USAGE: 'usage',
  ERROR: 'error',
  TASK_LINK: 'task_link',
  AGENT_SPAWNED: 'agent_spawned',
  VERIFICATION: 'verification',
  NOTE: 'note',
  /** chatflow: an ask-mode tool call waiting for the person (`{call_id, tool, args, classes,
   * question, state: pending|approved|denied, run_id}`), answered with `chatflow.approve`. */
  APPROVAL: 'approval',
  /** chatflow: the session's frozen prompt prefix (not drawn). */
  CONTEXT: 'context',
} as const;

// ─── chatflow / agents ──────────────────────────────────────────────────────

export interface TurnResult {
  run_id: string;
  queued_as_steer?: boolean;
}

export interface FlowState {
  session_id: string;
  running: boolean;
  run_id?: string | null;
  stage?: string | null;
  pending_steers: number;
  pending_followups: number;
}

export interface AgentRecord {
  name: string;
  project_id: string;
  role: string;
  charter?: string;
  status: string;
  session_id?: string | null;
  task_id?: string | null;
  workspace?: string | null;
  tasks_done?: number;
  tasks_failed?: number;
  updated_ms: number;
}

export interface ContractCheck {
  name: string;
  kind: string;
  spec: string;
  expect?: string | null;
}

export interface Task {
  id: string;
  project_id: string;
  parent_id?: string | null;
  title: string;
  brief: string;
  plan?: string;
  contract?: { checks: ContractCheck[] };
  status: string;
  assignee?: string | null;
  depends_on?: string[];
  touches?: string[];
  result?: string | null;
  session_id?: string | null;
  created_ms: number;
  updated_ms: number;
}

export interface BoardMessage {
  id: string;
  project_id: string;
  kind: string;
  author: string;
  text: string;
  refs?: string[];
  to?: string | null;
  ts_ms: number;
}

export interface CheckResult {
  name: string;
  ok: boolean;
  exit_code: number | null;
  output_tail: string;
  duration_ms: number;
}

export interface VerifyReport {
  task_id: string;
  ok: boolean;
  checks: CheckResult[];
  ts_ms: number;
}

// ─── vcs ────────────────────────────────────────────────────────────────────

export interface FileStatus {
  path: string;
  old_path?: string | null;
  status: string;
  staged: boolean;
  added?: number;
  removed?: number;
}

export interface VcsStatus {
  root: string;
  is_repo: boolean;
  branch: string;
  upstream?: string | null;
  ahead: number;
  behind: number;
  files: FileStatus[];
  added_lines: number;
  removed_lines: number;
  head?: string | null;
  detached?: boolean;
}

export interface DiffLine {
  kind: '+' | '-' | ' ';
  text: string;
  old_line?: number | null;
  new_line?: number | null;
  /** The line has no trailing newline (`\ No newline at end of file`). */
  no_eol?: boolean;
}

export interface Hunk {
  header: string;
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  lines: DiffLine[];
}

export interface FileDiff {
  path: string;
  old_path?: string | null;
  status: string;
  binary: boolean;
  hunks: Hunk[];
}

export interface Diff {
  files: FileDiff[];
  added: number;
  removed: number;
}

export interface BranchesResult {
  current: string;
  branches: string[];
}

export interface PrStatus {
  available: boolean;
  number?: number | null;
  url?: string | null;
  state?: string | null;
  ci?: string | null;
  detail?: string;
}

// ─── source (`protocol/src/contracts/source.rs`) ────────────────────────────

/** One file of `source.diff`: its unified diff text. */
export interface SourceFileDiff {
  path: string;
  /** added | modified | deleted | renamed */
  status: string;
  patch: string;
}

// ─── tools ──────────────────────────────────────────────────────────────────

export interface FileEntry {
  path: string;
  is_dir: boolean;
  size: number;
  /** Excluded by .gitignore / .ignore rules. */
  ignored?: boolean;
}

export interface ReadResult {
  text: string;
  total_lines?: number | null;
}

// ─── terminal ───────────────────────────────────────────────────────────────

export interface TermInfo {
  term_id: string;
  cwd: string;
  pid: number;
  cols: number;
  rows: number;
  alive: boolean;
}

export interface TermSnapshot {
  term_id: string;
  text: string;
  cursor_row: number;
  cursor_col: number;
  cols?: number;
  rows?: number;
  alive?: boolean;
  alternate_screen?: boolean;
  /** Only when requested by a renderer; agent snapshots stay plain text. */
  restore?: { data: string; offset: number } | null;
  input_base64?: boolean;
}

// ─── browser ────────────────────────────────────────────────────────────────

export interface BrowserTab {
  tab_id: string;
  url: string;
  title: string;
  loading: boolean;
}

export interface BrowserTabsEvent {
  tabs: BrowserTab[];
  active?: string | null;
}
