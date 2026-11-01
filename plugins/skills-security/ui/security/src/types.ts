/**
 * The memory plugin's security records (`memory.security.*`), as `protocol/src/contracts/memory.rs`
 * defines them (`Security*`). An `Option<T>` field is `T | null`, and may be absent from an older
 * peer; a `#[serde(default)]` scalar is always present.
 */

/** One audit run of a project by the security plugin. */
export interface SecurityAudit {
  id: string;
  started_ms: number;
  finished_ms?: number | null;
  /** Units (attack classes) the audit had, and lines it judged. */
  units: number;
  lines_judged: number;
  /** Confirmed findings, leads and hardening notes it produced. */
  findings: number;
  leads: number;
  hardening: number;
  /** The scope the audit was limited to (a path), when any. */
  scope?: string | null;
  skill_version: string;
  /** Where the Markdown export of the report was written, when it was. */
  report_path?: string | null;
  /** The map: the working tree read, its head, and what it holds (lists of scalars). */
  root?: string;
  git_head?: string;
  dirty?: boolean;
  files?: number;
  langs?: string[];
  stack?: string[];
  lockfiles?: string[];
  not_parsed?: string[];
  agent_files?: string[];
  /** When the dependency check ran, how many pinned packages it checked, why it did not run. */
  deps_checked_ms?: number | null;
  deps_packages?: number;
  deps_note?: string | null;
  /** The audit before this one; set when a newer audit began. */
  previous_id?: string | null;
  ended_ms?: number | null;
}

/** One step of a finding's trace, entry point first, sink last. */
export interface SecurityTraceStep {
  /** `entrypoint`, `step` or `sink`. */
  kind: string;
  file: string;
  line: number;
  what: string;
}

/** A fix the host can spell out exactly (today: a dependency upgrade). */
export interface SecurityQuickFix {
  /** `upgrade`. */
  kind: string;
  ecosystem: string;
  package: string;
  from: string;
  to: string;
  lockfile: string;
  /** The commands that make the change, in order. */
  commands: string[];
}

/** `confirmed`, `needs_validation`, `hardening` or `rejected`. */
export type SecurityVerdict = 'confirmed' | 'needs_validation' | 'hardening' | 'rejected';

/**
 * The lifecycle, moves the store allows: `open` -> `accepted` | `fixing` | `gone`, and `fixed` once
 * the host sees the sink gone; `fixing` -> `fixed` | `open`; `accepted` -> `open` | `gone`; `fixed`
 * -> `verified` | `open`; `verified` -> `open`; `gone` -> `open`. Accepting, not an issue, a reopen
 * and a forget are the person's; `verified` comes from the host's re-check.
 */
export type SecurityStatus = 'open' | 'fixing' | 'accepted' | 'fixed' | 'verified' | 'gone';

/** `user`, `model`, `agent` (a model's command), `recheck` (the host saw the code) or `host`. */
export type SecurityBy = 'user' | 'model' | 'agent' | 'recheck' | 'host';

/**
 * One security finding of a project: a confirmed vulnerability, a lead (one fact outside the code
 * decides) or a hardening note. Identified by its fingerprint (`<class>:<file>:<function>`).
 */
export interface SecurityFinding {
  id: string;
  class: string;
  verdict: SecurityVerdict | string;
  title: string;
  principal: string;
  control: string;
  impact: string;
  /** The demonstrated result, one line. */
  result: string;
  evidence_how: string;
  evidence_observed: string;
  severity_likelihood?: string | null;
  severity_impact?: string | null;
  /** `critical`, `high`, `medium` or `low` for confirmed findings. */
  severity_overall?: string | null;
  fix_where: string;
  fix_change: string;
  fix_test: string;
  /** For a lead: the fact outside the code that decides it. */
  missing_fact?: string | null;
  /** A lead's deciding fact and how the owner checks it, apart; a rejected claim's disproof. */
  blocker?: string | null;
  check_how?: string | null;
  reason?: string | null;
  conditions?: string[];
  trace: SecurityTraceStep[];
  quick_fix?: SecurityQuickFix | null;
  /** Computed by the writer, with the store's age term: higher first. */
  priority: number;
  priority_reason: string;
  /** Who found it: `audit` or `watch` (the daily dependency watch). */
  source?: string;
  /** The strong host signal behind it, and the severity the host set (a floor). */
  signal?: string | null;
  host_severity?: string | null;
  /** A dependency finding's advisories, CISA KEV listing and FIRST EPSS score (0 to 1). */
  advisories?: string[];
  kev?: boolean;
  epss?: number | null;
  epss_percentile?: number | null;
  // Lifecycle, owned by the store:
  status: SecurityStatus | string;
  status_note: string;
  status_by: SecurityBy | string;
  status_ms: number;
  first_seen_ms: number;
  last_seen_ms: number;
  /** How many audits reported it. */
  seen_in: number;
  audit_id?: string | null;
  /** The session that fixes or fixed it, and the commit that holds the fix. */
  session_id?: string | null;
  fixed_commit?: string | null;
  /** The sink's place (the trace's last step), for the list. */
  sink_path?: string | null;
  sink_line?: number | null;
  /** The sink's evidence the host checks a fix against (the line as shown, secrets masked). */
  sink_text?: string | null;
  sink_package?: string | null;
  sink_version?: string | null;
  notified_ms?: number | null;
}

/** One lifecycle move of a finding. */
export interface SecurityStatusChange {
  status: SecurityStatus | string;
  note: string;
  by: SecurityBy | string;
  ms: number;
  session_id?: string | null;
  commit?: string | null;
}

/** `memory.security.findings` parameters. */
export interface SecurityFindingsParams {
  project_id: string;
  /** Statuses to list (absent: open, fixing and accepted). */
  status?: string[];
  class?: string;
  min_priority?: number;
}

/** `memory.security.findings` result. */
export interface SecurityFindingsResult {
  findings: SecurityFinding[];
}

/** `memory.security.finding` and `memory.security.forget` parameters. */
export interface SecurityFindingParams {
  project_id: string;
  id: string;
}

/** `memory.security.finding` result. */
export interface SecurityFindingDetail {
  finding: SecurityFinding;
  history: SecurityStatusChange[];
}

/** `memory.security.finding.update` parameters. */
export interface SecurityFindingUpdateParams {
  project_id: string;
  id: string;
  status: SecurityStatus;
  note: string;
  by: SecurityBy;
  session_id?: string;
  commit?: string;
}

/** `memory.security.summary` result: the active list's shape and the last audit. */
export interface SecuritySummary {
  last_audit?: SecurityAudit | null;
  open: number;
  fixing: number;
  accepted: number;
  fixed: number;
  verified: number;
  /** Open and fixing findings by overall severity (`critical`, `high`, `medium`, `low`, `lead`, `hardening`). */
  by_severity: Record<string, number>;
  /** Open and fixing findings whose dependency is exploited in the wild (CISA KEV). */
  kev?: number;
}

/** `memory.security.recheck` result: what the host saw of each finding's sink. */
export interface SecurityRecheck {
  id: string;
  title: string;
  /** `unchanged`, `moved`, `gone` or `unknown`. */
  kind: string;
  line?: number | null;
  sentence: string;
  was: string;
  now?: string | null;
}

/** `memory.security.changed` payload. */
export interface SecurityChanged {
  project_id: string;
  id?: string | null;
  status?: string | null;
}
