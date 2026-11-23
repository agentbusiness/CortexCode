//! The security sentinel: what the security skills plugin (`skills-security`) serves beside its
//! skills, on its own category (`skills-security.<verb>`; the frame routes by the prefix). It keeps
//! every project of this computer watched and decides what reaches the person:
//!
//! - **Sweeps.** A delta sweep every few minutes reads which advisories changed since the last one
//!   (OSV's export of modified records, the malicious-package records first) and re-checks only the
//!   pinned packages they name, across every project at once; a full sweep (daily, at start, on
//!   demand) re-reads every project's lockfiles, the supply-chain signals (install hooks, planted
//!   workflows, known campaign indicators, release-cooldown and install-script posture), the CI
//!   workflows, the AI-agent files and the credentials in the tree, and checks every pinned package
//!   against OSV, CISA KEV and FIRST EPSS in one batch for the whole computer. Findings go to each
//!   project's graph through `memory.security.findings.put` (source `sweep`).
//! - **The filter.** Every finding not judged yet is judged once: first by host rules on structural
//!   facts (a malicious package, a campaign indicator, a live credential or an exploited
//!   vulnerability the code imports is an alert; a mute rule silences; a development-only package
//!   with a low severity is quiet), then by the local model in one short constrained call with the
//!   attack class's decision card (`data/triage/<class>.md`). What the model cannot settle is
//!   investigated by a session that reads the code and records a verdict with its evidence.
//! - **Notifications.** Only alerts reach the person: at once when critical, malicious, exploited,
//!   a campaign indicator or a live credential; the rest in one daily digest. Quiet and muted
//!   findings never notify.
//!
//! Records live in the graph (`docs/GRAPH-MODEL.md`, "Security sentinel"): the feed cursors and
//! the last sweep on `SecuritySentinel` and `SecurityFeed` nodes, mute rules as `SecurityMute`
//! nodes, all on the `Machine`.

use serde::{Deserialize, Serialize};

/// The category the sentinel's methods are served on (the security skills plugin's).
pub const CATEGORY: &str = "skills-security";

pub mod methods {
    /// `{}` -> [`super::SentinelStatus`]: the running sweep, the last one, the feeds' freshness.
    pub const STATUS: &str = "skills-security.status";
    /// `{}` -> [`super::Fleet`]: every project's alert counts and posture, and the problems shared
    /// by several projects (one upgrade fixes them all).
    pub const FLEET: &str = "skills-security.fleet";
    /// Start a sweep now: [`super::SweepParams`] -> [`super::SweepStarted`]. Progress follows on
    /// [`super::topics::PROGRESS`].
    pub const SWEEP: &str = "skills-security.sweep";
    /// Judge findings now: [`super::TriageParams`] -> [`super::TriageResult`].
    pub const TRIAGE: &str = "skills-security.triage";
    /// Start the investigation of one finding: [`super::InvestigateParams`] ->
    /// [`super::InvestigateResult`].
    pub const INVESTIGATE: &str = "skills-security.investigate";
    /// Add a mute rule from a finding: [`super::MuteAddParams`] -> [`super::MuteRule`]. The
    /// findings it covers in every project are muted at once.
    pub const MUTE_ADD: &str = "skills-security.mute.add";
    /// `{}` -> [`super::MuteListResult`].
    pub const MUTE_LIST: &str = "skills-security.mute.list";
    /// Remove a mute rule: [`super::MuteRemoveParams`] -> `{}`. The findings it muted are judged
    /// again.
    pub const MUTE_REMOVE: &str = "skills-security.mute.remove";
    /// Which projects a package, a version, an advisory or a campaign indicator touches, read from
    /// the lockfiles and installed packages now: [`super::AffectedParams`] ->
    /// [`super::AffectedResult`].
    pub const AFFECTED: &str = "skills-security.affected";
    /// The filter for another app's security events (a process, a connection, a file, a login):
    /// [`super::JudgeParams`] -> [`super::JudgeResult`], one short judgment each.
    pub const JUDGE: &str = "skills-security.judge";
    pub const ALL: &[&str] = &[STATUS, FLEET, SWEEP, TRIAGE, INVESTIGATE, MUTE_ADD, MUTE_LIST, MUTE_REMOVE, AFFECTED, JUDGE];
}

pub mod topics {
    /// [`super::SweepProgress`] while a sweep runs and once when it ends.
    pub const PROGRESS: &str = "skills-security.progress";
    /// `{}`: the fleet's picture changed (a sweep ended, a mute rule came or went).
    pub const CHANGED: &str = "skills-security.changed";
}

/// [`methods::SWEEP`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SweepParams {
    /// Only this project (absent: every project).
    #[serde(default)]
    pub project_id: Option<String>,
    /// A full sweep (every check, every package); absent or false: the delta sweep.
    #[serde(default)]
    pub full: bool,
}

/// [`methods::SWEEP`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SweepStarted {
    /// A sweep began now.
    pub started: bool,
    /// One was already running (it covers this request when `queued` is false).
    #[serde(default)]
    pub running: bool,
    /// The request runs after the running one.
    #[serde(default)]
    pub queued: bool,
}

/// [`topics::PROGRESS`] payload, and the running sweep in [`SentinelStatus`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SweepProgress {
    /// `delta`, `full` or `project`.
    pub kind: String,
    /// `projects`, `advisories`, `findings`, `triage`, `done` or `failed`.
    pub phase: String,
    #[serde(default)]
    pub done: u32,
    #[serde(default)]
    pub total: u32,
    /// What is being done, in words (`checking 1,204 packages of 31 projects`).
    #[serde(default)]
    pub note: String,
    pub started_ms: u64,
}

/// One live source and how fresh it is.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FeedState {
    /// `osv` (the vulnerability and malicious-package records), `kev`, `epss`.
    pub name: String,
    /// When it last answered.
    #[serde(default)]
    pub checked_ms: Option<u64>,
    /// Whether the last read worked.
    #[serde(default)]
    pub ok: bool,
    /// Why not, or what it brought (`12 changed advisories since 10:40`).
    #[serde(default)]
    pub note: String,
}

/// One finished sweep.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SweepRecord {
    pub kind: String,
    pub started_ms: u64,
    pub finished_ms: u64,
    #[serde(default)]
    pub projects: u32,
    #[serde(default)]
    pub packages: u32,
    /// Findings written (new or changed).
    #[serde(default)]
    pub findings: u32,
    /// Judged as alerts and as quiet in this sweep.
    #[serde(default)]
    pub alerts: u32,
    #[serde(default)]
    pub quiet: u32,
    /// What failed, one line each (a project that could not be read, a source that did not answer).
    #[serde(default)]
    pub errors: Vec<String>,
}

/// [`methods::STATUS`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SentinelStatus {
    #[serde(default)]
    pub running: Option<SweepProgress>,
    #[serde(default)]
    pub last: Option<SweepRecord>,
    /// When the next delta and full sweeps are due.
    #[serde(default)]
    pub next_delta_ms: Option<u64>,
    #[serde(default)]
    pub next_full_ms: Option<u64>,
    #[serde(default)]
    pub feeds: Vec<FeedState>,
    /// Findings waiting for the model's judgment, and the investigations running or queued.
    #[serde(default)]
    pub judging: u32,
    #[serde(default)]
    pub investigations: u32,
    /// Campaign persistence found on this computer itself, outside any project, one line each.
    #[serde(default)]
    pub machine: Vec<String>,
}

/// One project in [`Fleet`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FleetProject {
    pub project_id: String,
    pub name: String,
    pub root: String,
    /// Open and fixing findings, and how the filter judged them.
    #[serde(default)]
    pub open: u32,
    #[serde(default)]
    pub alerts: u32,
    #[serde(default)]
    pub investigating: u32,
    #[serde(default)]
    pub quiet: u32,
    #[serde(default)]
    pub pending: u32,
    /// Alerts by severity (`critical`, `high`, ...).
    #[serde(default)]
    pub by_severity: std::collections::BTreeMap<String, u32>,
    /// Alerts that are a malicious package, a campaign indicator, an exploited vulnerability or a
    /// live credential.
    #[serde(default)]
    pub urgent: u32,
    /// The supply-chain posture checks that fail, in words (`no release cooldown for npm`).
    #[serde(default)]
    pub posture: Vec<String>,
    #[serde(default)]
    pub last_audit_ms: Option<u64>,
    /// Why the project could not be read, when it could not.
    #[serde(default)]
    pub error: Option<String>,
}

/// A problem several projects share (one dependency version, one indicator, one posture gap).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FleetGroup {
    /// The findings' `fleet_key`.
    pub key: String,
    pub title: String,
    /// The worst severity among its alerts.
    #[serde(default)]
    pub severity: String,
    /// `alert` when any of its findings is an alert, else the shared judgment.
    #[serde(default)]
    pub triage: String,
    pub project_ids: Vec<String>,
    /// The upgrade that fixes it everywhere, when one exists (`lodash 4.17.21`).
    #[serde(default)]
    pub fix: Option<String>,
}

/// [`methods::FLEET`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Fleet {
    pub projects: Vec<FleetProject>,
    /// Shared by two projects or more, alerts first.
    pub groups: Vec<FleetGroup>,
    pub status: SentinelStatus,
}

/// [`methods::TRIAGE`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TriageParams {
    pub project_id: String,
    /// These findings (absent: every one not judged yet).
    #[serde(default)]
    pub ids: Option<Vec<String>>,
    /// Judge again even when judged (a person's "look again").
    #[serde(default)]
    pub again: bool,
}

/// [`methods::TRIAGE`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TriageResult {
    pub judged: u32,
    #[serde(default)]
    pub alerts: u32,
    #[serde(default)]
    pub quiet: u32,
    #[serde(default)]
    pub investigating: u32,
}

/// [`methods::INVESTIGATE`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InvestigateParams {
    pub project_id: String,
    pub id: String,
}

/// [`methods::INVESTIGATE`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InvestigateResult {
    /// The session that investigates; None when it waits behind a running one.
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub queued: bool,
}

/// A rule that silences findings: one finding, the same problem everywhere, a package, an advisory
/// or a class under a path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MuteRule {
    pub key: String,
    /// `finding` (this finding of this project), `similar` (the same `fleet_key` in every project),
    /// `package` (this package in every version and project), `advisory` (this advisory
    /// everywhere) or `path` (this class under this path of this project).
    pub kind: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub finding_id: Option<String>,
    #[serde(default)]
    pub fleet_key: Option<String>,
    #[serde(default)]
    pub ecosystem: Option<String>,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub advisory: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Why, in the person's words.
    #[serde(default)]
    pub note: String,
    /// What the rule covers, in words (`lodash in every project`).
    #[serde(default)]
    pub label: String,
    pub created_ms: u64,
    /// The rule ends then (absent: it stays).
    #[serde(default)]
    pub until_ms: Option<u64>,
}

/// [`methods::MUTE_ADD`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MuteAddParams {
    pub project_id: String,
    pub finding_id: String,
    /// As [`MuteRule::kind`].
    pub kind: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub until_ms: Option<u64>,
}

/// [`methods::MUTE_LIST`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MuteListResult {
    pub rules: Vec<MuteRule>,
}

/// [`methods::MUTE_REMOVE`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MuteRemoveParams {
    pub key: String,
}

/// [`methods::AFFECTED`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AffectedParams {
    /// A package (`lodash`, `npm:lodash`), a version (`lodash@4.17.20`), an advisory (`GHSA-…`,
    /// `CVE-…`, `MAL-…`) or a campaign indicator (a file name such as `setup_bun.js`).
    pub query: String,
}

/// One place an [`AffectedParams`] query touches.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AffectedHit {
    pub project_id: String,
    pub name: String,
    /// What matched, in words (`lodash 4.17.20 pinned`, `setup_bun.js in node_modules/x`).
    pub what: String,
    /// The file it is in, relative to the project.
    #[serde(default)]
    pub path: String,
}

/// [`methods::AFFECTED`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AffectedResult {
    pub query: String,
    pub hits: Vec<AffectedHit>,
    /// Projects read.
    #[serde(default)]
    pub checked: u32,
    /// What to do next, in words (the credentials to rotate when a malicious package was
    /// installed), or why the answer is partial.
    #[serde(default)]
    pub note: String,
}

/// One event another app asks the filter about.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JudgeItem {
    /// The caller's id for it, given back with the verdict.
    pub id: String,
    /// `process`, `network`, `file`, `package`, `config`, `login`, `persistence` or `other`.
    pub kind: String,
    pub title: String,
    /// The facts the caller has, one per entry (`signed by Apple`, `listens on 0.0.0.0:5000`).
    #[serde(default)]
    pub facts: Vec<String>,
}

/// [`methods::JUDGE`] parameters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JudgeParams {
    pub items: Vec<JudgeItem>,
}

/// One verdict in [`JudgeResult`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JudgeVerdict {
    pub id: String,
    /// `dismiss` (normal behaviour), `note` (worth a line, not an alert), `alert` (tell the person)
    /// or `investigate` (look deeper before deciding).
    pub decision: String,
    pub reason: String,
}

/// [`methods::JUDGE`] result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JudgeResult {
    pub verdicts: Vec<JudgeVerdict>,
}
