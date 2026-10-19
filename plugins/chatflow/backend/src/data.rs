//! Every model-facing text and tunable of the chat flow, as data loaded at runtime.
//!
//! The set is three kinds of files: `stages.toml` (stage order, sampling profiles, effort and limits
//! per stage), `texts.toml` (short templates with `{placeholders}`), `prompts/*.md` (policy and
//! stage rules, with front-matter) and `schemas/*.json` (the constrained stages' JSON Schemas;
//! property order is the `required` order). Three layers, later ones winning per file:
//! 1. the shipped files compiled into the binary (always valid; the frame runs a copy of the binary);
//! 2. a shipped data directory named by the plugin config (`data_dir`);
//! 3. the live data: `backend/data` in the plugin's folder of the code folder, where the app and
//!    the user edit it (absent in safe mode; it may appear while the plugin runs). It never gives
//!    the [`PROTECTED`] files.
//!
//! A set is validated as a whole on load: every file parses, every text keeps exactly the
//! placeholders of its shipped version, every stage names existing rules and a schema the flow can
//! read. A set that fails is rejected and the previous one stays active; both outcomes are
//! published as `chatflow.data` events. Each set has a version (label plus content hash) that runs
//! record in their status entries, so results can be attributed to a prompt version.

use crate::schemas::{self, Shape};
use cc_protocol::contracts::model::Sampling;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

/// The label of the live layer (see the module doc).
pub const LIVE: &str = "live";

/// Files the live layer never gives: the safety policy is data, but a change to it needs the user's
/// approval like a code change, so a repository the app works in cannot rewrite it.
const PROTECTED: &[&str] = &["prompts/policy.md"];

/// The shipped files (relative path, content), compiled in.
const EMBEDDED: &[(&str, &str)] = &[
    ("stages.toml", include_str!("../data/stages.toml")),
    ("texts.toml", include_str!("../data/texts.toml")),
    ("prompts/policy.md", include_str!("../data/prompts/policy.md")),
    ("prompts/route.md", include_str!("../data/prompts/route.md")),
    ("prompts/decide.md", include_str!("../data/prompts/decide.md")),
    ("prompts/checks.md", include_str!("../data/prompts/checks.md")),
    ("prompts/reply.md", include_str!("../data/prompts/reply.md")),
    ("prompts/do.md", include_str!("../data/prompts/do.md")),
    ("prompts/task.md", include_str!("../data/prompts/task.md")),
    ("prompts/ask.md", include_str!("../data/prompts/ask.md")),
    ("prompts/spec.md", include_str!("../data/prompts/spec.md")),
    ("prompts/spec_edit.md", include_str!("../data/prompts/spec_edit.md")),
    ("prompts/plan.md", include_str!("../data/prompts/plan.md")),
    ("prompts/plan_edit.md", include_str!("../data/prompts/plan_edit.md")),
    ("prompts/decisions.md", include_str!("../data/prompts/decisions.md")),
    ("prompts/describe.md", include_str!("../data/prompts/describe.md")),
    ("prompts/knowledge.md", include_str!("../data/prompts/knowledge.md")),
    ("prompts/check_rules.md", include_str!("../data/prompts/check_rules.md")),
    // EXPERIMENT (2026-10-19, rule checks): see `[stage.DO]` in stages.toml to switch it off.
    ("prompts/experiment_rule_checks.md", include_str!("../data/prompts/experiment_rule_checks.md")),
    ("prompts/plan_fields.md", include_str!("../data/prompts/plan_fields.md")),
    ("prompts/review.md", include_str!("../data/prompts/review.md")),
    ("schemas/review.json", include_str!("../data/schemas/review.json")),
    ("schemas/router.json", include_str!("../data/schemas/router.json")),
    ("schemas/decide.json", include_str!("../data/schemas/decide.json")),
    ("schemas/checks.json", include_str!("../data/schemas/checks.json")),
    ("schemas/spec.json", include_str!("../data/schemas/spec.json")),
    ("schemas/spec_edit.json", include_str!("../data/schemas/spec_edit.json")),
    ("schemas/plan.json", include_str!("../data/schemas/plan.json")),
    ("schemas/decisions.json", include_str!("../data/schemas/decisions.json")),
    ("schemas/knowledge.json", include_str!("../data/schemas/knowledge.json")),
];

/// Roles a session's system prompt is built for: `chat` (the foreground flow) and `agent` (a
/// background agent working on one task).
pub const ROLES: &[&str] = &[ROLE_CHAT, ROLE_AGENT];
pub const ROLE_CHAT: &str = "chat";
pub const ROLE_AGENT: &str = "agent";
/// Stages the flow runs; each must be configured.
pub const STAGES: &[&str] = &["ROUTE", "DECIDE", "CHECKS", "REPLY", "DO", "TASK", "ASK", "SPEC", "SPEC_EDIT", "PLAN", "PLAN_EDIT", "DECISIONS", "KNOWLEDGE", "DESCRIBE", "REVIEW"];
/// Fields the flow reads from each constrained stage's output, and the values it can act on.
/// (stage, fields it reads, the enum field and the values the flow can act on).
type SchemaNeed = (&'static str, &'static [&'static str], Option<(&'static str, &'static [&'static str])>);
const SCHEMA_NEEDS: &[SchemaNeed] = &[
    ("ROUTE", &["work", "route"], Some(("route", &["ANSWER", "LOOKUP", "DO", "DELEGATE", "STATUS", "PLAN", "IMPLEMENT"]))),
    ("DECIDE", &["missing", "route"], Some(("route", &["MORE", "ANSWER", "DO", "DELEGATE", "PLAN"]))),
    ("CHECKS", &["checks"], None),
    ("SPEC", &["title", "requirements"], None),
    ("SPEC_EDIT", &["add", "drop", "modify"], None),
    ("PLAN", &["goal", "tasks"], None),
    ("PLAN_EDIT", &["goal", "tasks"], None),
    ("DECISIONS", &["decisions"], None),
    ("KNOWLEDGE", &["items"], None),
    ("REVIEW", &["found", "replan"], Some(("replan", &["no", "yes"]))),
];
const EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStages {
    label: String,
    protocol: BTreeMap<String, Vec<String>>,
    sampling: BTreeMap<String, RawSampling>,
    effort: EffortConfig,
    limits: Limits,
    stage: BTreeMap<String, RawStage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSampling {
    temperature: f32,
    top_p: f32,
    top_k: u32,
    #[serde(default)]
    min_p: f32,
    #[serde(default)]
    presence_penalty: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffortConfig {
    pub default: String,
    pub escalate: String,
    /// The thinking of a plan task's loop (execution of a planned step).
    #[serde(default = "default_task_effort")]
    pub task: String,
}

fn default_task_effort() -> String {
    "low".to_owned()
}

fn default_replans() -> u32 {
    2
}

fn default_task_calls() -> u32 {
    0
}

fn default_fix_think_tokens() -> u32 {
    384
}

fn default_same_error_edits() -> u32 {
    3
}

fn default_fix_cycles() -> u32 {
    3
}

fn default_context_tokens() -> u32 {
    0
}

fn default_stall_runs() -> u32 {
    20
}

fn default_task_think_tokens() -> u32 {
    1024
}

fn default_loop_think_tokens() -> u32 {
    1024
}

fn default_reasoning_steps() -> u32 {
    0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub read_rounds: usize,
    pub check_rounds: usize,
    pub repeat_at: u32,
    pub repo_map_share: u32,
    pub repo_map_min: u32,
    pub repo_map_max: u32,
    pub focus_map_tokens: u32,
    pub search_limit: u32,
    pub hibernate_after_ms: u64,
    /// Reads at most this many items per read round (follow-ups included).
    pub reads_per_round: usize,
    /// Tokens of one conversation's context kept free for the answer: a prompt is folded until it
    /// fits the rest.
    pub answer_reserve_tokens: u32,
    /// A compaction brings the prompt down to at most this many tokens; one that cannot is not
    /// made (the next layer is). A fixed size, not a share of the window: what the model works
    /// from stays small whatever the window holds (seen: shares of a 230k window let a turn grow to
    /// 220k, each compaction saving 2k and costing a 15-minute re-read).
    #[serde(default = "compact_target_tokens")]
    pub compact_target_tokens: u32,
    /// At a wait (the run's tools executing) a prompt past this many tokens is compacted then and
    /// its re-read runs in the background ([`crate::fit::prepare`]). 0 = never.
    #[serde(default = "compact_early_tokens")]
    pub compact_early_tokens: u32,
    /// When a turn ends (the user reads the answer, the model is idle) a prompt past this many
    /// tokens is compacted then, so the next turn starts with room. 0 = only at the early size.
    #[serde(default = "compact_idle_tokens")]
    pub compact_idle_tokens: u32,
    /// Steps of an agent loop between settles: a settle applies the in-place rewrites (outdated
    /// reads, earlier edit echoes and run outputs of a command run again) up to the newest entry.
    /// Between settles the prompt only grows, so the engine continues its state; a settle makes it
    /// re-read the (small) rewritten history once. 0 = only at compactions.
    #[serde(default = "default_settle_steps")]
    pub settle_steps: u32,
    /// A settle is made only when it shrinks the prompt by at least this many tokens: it costs one
    /// re-read of the history after the system block.
    #[serde(default = "default_settle_min_tokens")]
    pub settle_min_tokens: u32,
    /// Steps of the running turn a compaction keeps whole (the newest); older steps of it may become
    /// one note listing their calls when folding outputs is not enough (see [`crate::fit`]).
    #[serde(default = "default_history_tail_steps")]
    pub history_tail_steps: u32,
    /// Skills a decision may choose for its work; 0 removes the choice (the model reads on its own).
    pub skills_per_turn: usize,
    /// Times the plan of a decided change is written again after a task ended badly, per turn.
    #[serde(default = "default_replans")]
    pub replans: u32,
    /// Tool calls one plan task may make before it counts as stuck: its loop ends and the tasks
    /// still to do are written again with another approach (within `replans`).
    #[serde(default = "default_task_calls")]
    pub task_calls: u32,
    /// Most thinking tokens per step of a plan task (0 = the effort's default budget).
    #[serde(default = "default_task_think_tokens")]
    pub task_think_tokens: u32,
    /// Most thinking tokens per step once the task has changed a file (a fix, not the design).
    #[serde(default = "default_fix_think_tokens")]
    pub fix_think_tokens: u32,
    /// Most thinking tokens per step of any other agent loop (a question, a follow-up): every
    /// generating call carries a budget, so the model plugin's own effort table (4096 for medium)
    /// is never what a step gets (seen: a loop without a plan step ran 4k-token thoughts for hours).
    #[serde(default = "default_loop_think_tokens")]
    pub loop_think_tokens: u32,
    /// Steps, newest first, whose reasoning the prompt still carries; older steps show their answer
    /// and calls only, as the model's own template renders earlier turns (seen: 4k tokens of
    /// thinking per step kept for 280 steps filled the window). The engine resumes from the
    /// checkpoint it took before generating, so a dropped thought costs no re-read of its own.
    #[serde(default = "default_reasoning_steps")]
    pub reasoning_steps: u32,
    /// Edit-then-rerun cycles after which the same command still failing makes a plan task stuck.
    #[serde(default = "default_fix_cycles")]
    pub fix_cycles: u32,
    /// Failing runs the same error may come back in (with other calls between) before the host
    /// says its cause is not on its line ([`crate::typeerrors`]); 0 = never.
    #[serde(default = "default_same_error_edits")]
    pub same_error_edits: u32,
    /// Commands run while a check fails, none of them turning a failing command to pass, after which
    /// a plan task is stuck (0 = no limit): debugging runs and traces that never land (fix cycles
    /// alone miss them, as no edit sits between them).
    #[serde(default = "default_stall_runs")]
    pub stall_runs: u32,
    /// Tokens one conversation may use at most, whatever the model could hold (0 = its window):
    /// the compaction shares apply to this.
    #[serde(default = "default_context_tokens")]
    pub context_tokens: u32,
    /// The skills catalog is part of a chat session's system message (session order; cached across
    /// turns and prewarmed) instead of the choosing stages' trailing message (previous turn's
    /// skills first; re-read by every ROUTE). The previous turn's skills are named in ROUTE's stage
    /// message either way.
    #[serde(default = "skills_in_prefix")]
    pub skills_in_prefix: bool,
    /// A message whose text is longer than this (characters) becomes the spec when none exists;
    /// 0 = only when marked or asked for.
    #[serde(default = "spec_min_chars")]
    pub spec_min_chars: usize,
    /// Spec text pinned whole at most (characters).
    #[serde(default = "spec_max_chars")]
    pub spec_max_chars: usize,
    /// Tasks one plan may hold.
    #[serde(default = "plan_max_tasks")]
    pub plan_max_tasks: usize,
    /// Decisions one DECISIONS stage may propose.
    #[serde(default = "decisions_per_turn")]
    pub decisions_per_turn: usize,
    /// A workspace-wide diagnostics read before DECIDE lists at most this many per server (errors
    /// first, the rest counted).
    #[serde(default = "diagnostics_shown")]
    pub diagnostics_shown: u32,
    /// Knowledge items one KNOWLEDGE stage may propose (docs/PROJECT-KNOWLEDGE-PLAN.md P3).
    #[serde(default = "knowledge_items_per_turn")]
    pub knowledge_items_per_turn: usize,
    /// Knowledge items read by meaning for a turn's work and shown as one host note; 0 = none (P4).
    #[serde(default = "knowledge_per_turn")]
    pub knowledge_per_turn: u32,
    /// The project's knowledge brief in the system prompt, in tokens; 0 = none (P4).
    #[serde(default = "brief_tokens")]
    pub brief_tokens: u32,
    /// The project's own contract (its `AGENTS.md`, else `CLAUDE.md`) in the system prompt, in
    /// characters; 0 = none.
    #[serde(default = "contract_chars")]
    pub contract_chars: usize,
}

// Defaults for limits added after the first data sets (an older override stays valid).
fn spec_min_chars() -> usize {
    0
}
fn spec_max_chars() -> usize {
    24000
}
fn plan_max_tasks() -> usize {
    12
}
fn decisions_per_turn() -> usize {
    3
}
fn diagnostics_shown() -> u32 {
    12
}
fn knowledge_items_per_turn() -> usize {
    3
}
fn knowledge_per_turn() -> u32 {
    5
}
fn contract_chars() -> usize {
    6000
}

fn brief_tokens() -> u32 {
    1024
}
fn skills_in_prefix() -> bool {
    true
}
fn compact_target_tokens() -> u32 {
    24576
}
fn compact_early_tokens() -> u32 {
    49152
}
fn compact_idle_tokens() -> u32 {
    32768
}
fn default_settle_steps() -> u32 {
    8
}
fn default_settle_min_tokens() -> u32 {
    3072
}
fn default_history_tail_steps() -> u32 {
    8
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStage {
    rules: Rules,
    #[serde(default)]
    schema: Option<String>,
    effort: String,
    #[serde(default = "auto")]
    sampling: String,
}

/// A stage's rules: one prompt id, or several joined in order (parts several stages share, such as
/// the check rules, are written once).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Rules {
    One(String),
    Parts(Vec<String>),
}

impl Rules {
    fn parts(&self) -> Vec<String> {
        match self {
            Rules::One(id) => vec![id.clone()],
            Rules::Parts(ids) => ids.clone(),
        }
    }
}

fn auto() -> String {
    "auto".to_owned()
}

#[derive(Debug, Clone)]
pub struct StageConfig {
    /// Prompt ids of the stage's rules, in order.
    pub rules: Vec<String>,
    pub shape: Option<Shape>,
    /// A fixed level, or `decided`.
    pub effort: String,
    pub sampling: String,
}

/// One validated data set.
#[derive(Debug, Clone)]
pub struct Dataset {
    pub label: String,
    pub hash: String,
    /// Where each file came from (`embedded`, `shipped`, `override`).
    pub sources: BTreeMap<String, String>,
    /// Per role, the stages whose rules its system prompt carries (in order).
    pub protocol: BTreeMap<String, Vec<String>>,
    pub effort: EffortConfig,
    pub limits: Limits,
    stages: HashMap<String, StageConfig>,
    sampling: HashMap<String, Sampling>,
    texts: HashMap<String, String>,
    prompts: HashMap<String, String>,
}

/// `{name}` placeholders of a text (mechanical scan).
pub fn placeholders(text: &str) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) if !after[..close].is_empty() && after[..close].chars().all(|c| c.is_ascii_lowercase() || c == '_') => {
                found.insert(after[..close].to_owned());
                rest = &after[close + 1..];
            }
            _ => rest = after,
        }
    }
    found
}

/// FNV-1a 64 (mechanical content fingerprint).
fn fnv(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Split `---\nkey: value\n---\nbody` (mechanical front-matter parse).
fn front_matter(text: &str) -> Result<(HashMap<String, String>, String), String> {
    let Some(rest) = text.strip_prefix("---\n") else { return Err("missing front-matter (--- ... ---)".into()) };
    let Some(end) = rest.find("\n---\n") else { return Err("unterminated front-matter".into()) };
    let mut meta = HashMap::new();
    for line in rest[..end].lines().filter(|line| !line.trim().is_empty()) {
        let (key, value) = line.split_once(':').ok_or_else(|| format!("front-matter line without `key: value`: {line}"))?;
        meta.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    Ok((meta, rest[end + 5..].trim_end().to_owned()))
}

fn embedded_files() -> BTreeMap<String, (String, String)> {
    EMBEDDED.iter().map(|(name, text)| ((*name).to_owned(), ((*text).to_owned(), "embedded".to_owned()))).collect()
}

/// Data files of a directory layer labelled `label` (only the kinds the set uses; none when the
/// directory does not exist).
fn layer_files(label: &str, dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for name in ["stages.toml", "texts.toml"] {
        let path = dir.join(name);
        if path.is_file() {
            out.push((name.to_owned(), path));
        }
    }
    for (sub, extension) in [("prompts", "md"), ("schemas", "json")] {
        let Ok(entries) = std::fs::read_dir(dir.join(sub)) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some(extension) {
                let file = entry.file_name().to_string_lossy().to_string();
                out.push((format!("{sub}/{file}"), path));
            }
        }
    }
    if label == LIVE {
        out.retain(|(name, _)| !PROTECTED.contains(&name.as_str()));
    }
    out.sort();
    out
}

/// The effective files: embedded, then each directory layer over it.
pub fn gather(layers: &[(&str, &Path)]) -> Result<BTreeMap<String, (String, String)>, String> {
    let mut files = embedded_files();
    for (label, dir) in layers {
        for (name, path) in layer_files(label, dir) {
            let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            files.insert(name, (text, (*label).to_owned()));
        }
    }
    Ok(files)
}

impl Dataset {
    /// The shipped set (cannot fail in a correct build; a test checks it).
    pub fn embedded() -> Result<Self, String> {
        Self::build(&embedded_files())
    }

    /// Validate and build a set from its files.
    pub fn build(files: &BTreeMap<String, (String, String)>) -> Result<Self, String> {
        let text_of = |name: &str| files.get(name).map(|(text, _)| text.as_str()).ok_or_else(|| format!("{name} is missing"));
        let raw: RawStages = toml::from_str(text_of("stages.toml")?).map_err(|error| format!("stages.toml: {error}"))?;

        // Texts: the same keys and placeholders as the shipped version.
        let texts: HashMap<String, String> = toml::from_str(text_of("texts.toml")?).map_err(|error| format!("texts.toml: {error}"))?;
        let shipped: HashMap<String, String> =
            toml::from_str(EMBEDDED.iter().find(|(name, _)| *name == "texts.toml").map(|(_, text)| *text).unwrap_or(""))
                .map_err(|error| format!("shipped texts.toml: {error}"))?;
        for (key, shipped_text) in &shipped {
            let Some(text) = texts.get(key) else { return Err(format!("texts.toml: `{key}` is missing")) };
            let (want, have) = (placeholders(shipped_text), placeholders(text));
            if want != have {
                let mut missing: Vec<&String> = want.difference(&have).collect();
                let mut unknown: Vec<&String> = have.difference(&want).collect();
                missing.sort();
                unknown.sort();
                return Err(format!("texts.toml: `{key}` must use the placeholders {want:?} (missing {missing:?}, unknown {unknown:?})"));
            }
        }

        // Prompts: front-matter id = file name, no placeholders (prompts are not filled in).
        let mut prompts = HashMap::new();
        for (name, (text, _)) in files.iter().filter(|(name, _)| name.starts_with("prompts/")) {
            let (meta, body) = front_matter(text).map_err(|error| format!("{name}: {error}"))?;
            let stem = name.trim_start_matches("prompts/").trim_end_matches(".md");
            if meta.get("id").map(String::as_str) != Some(stem) {
                return Err(format!("{name}: front-matter `id` must be `{stem}`"));
            }
            if body.trim().is_empty() {
                return Err(format!("{name}: empty prompt"));
            }
            if let Some(found) = placeholders(&body).into_iter().next() {
                return Err(format!("{name}: prompts take no placeholders (found {{{found}}})"));
            }
            prompts.insert(stem.to_owned(), body);
        }
        if !prompts.contains_key("policy") {
            return Err("prompts/policy.md is missing".into());
        }

        // Sampling profiles.
        let mut sampling = HashMap::new();
        for (name, profile) in &raw.sampling {
            if !(0.0..=2.0).contains(&profile.temperature) || !(0.0..=1.0).contains(&profile.top_p) {
                return Err(format!("stages.toml: sampling `{name}` is out of range"));
            }
            sampling.insert(
                name.clone(),
                Sampling {
                    temperature: profile.temperature,
                    top_p: profile.top_p,
                    top_k: profile.top_k,
                    min_p: profile.min_p,
                    presence_penalty: profile.presence_penalty,
                    repeat_penalty: None,
                    seed: None,
                },
            );
        }
        for needed in ["thinking", "plain"] {
            if !sampling.contains_key(needed) {
                return Err(format!("stages.toml: sampling profile `{needed}` is missing"));
            }
        }
        for level in [&raw.effort.default, &raw.effort.escalate, &raw.effort.task] {
            if !EFFORTS.contains(&level.as_str()) {
                return Err(format!("stages.toml: unknown effort `{level}`"));
            }
        }
        let limits = &raw.limits;
        if limits.read_rounds == 0 || limits.check_rounds == 0 || limits.repeat_at < 2 || limits.repo_map_share == 0 || limits.repo_map_min > limits.repo_map_max || limits.reads_per_round == 0 || limits.answer_reserve_tokens == 0 || limits.plan_max_tasks == 0 || limits.spec_max_chars == 0 {
            return Err("stages.toml: a limit is out of range".into());
        }

        // Stages.
        let mut stages = HashMap::new();
        for name in STAGES {
            let Some(stage) = raw.stage.get(*name) else { return Err(format!("stages.toml: [stage.{name}] is missing")) };
            let rules = stage.rules.parts();
            if rules.is_empty() {
                return Err(format!("stages.toml: stage {name} names no rules"));
            }
            if let Some(missing) = rules.iter().find(|id| !prompts.contains_key(*id)) {
                return Err(format!("stages.toml: stage {name} names rules `{missing}` but prompts/{missing}.md does not exist"));
            }
            if stage.effort != "decided" && !EFFORTS.contains(&stage.effort.as_str()) {
                return Err(format!("stages.toml: stage {name} has unknown effort `{}`", stage.effort));
            }
            if stage.sampling != "auto" && !sampling.contains_key(&stage.sampling) {
                return Err(format!("stages.toml: stage {name} names unknown sampling `{}`", stage.sampling));
            }
            let shape = match &stage.schema {
                Some(path) => {
                    let text = text_of(path).map_err(|error| format!("stage {name}: {error}"))?;
                    let value: Value = serde_json::from_str(text).map_err(|error| format!("{path}: {error}"))?;
                    let shape = schemas::shape_from_schema(&value).map_err(|error| format!("{path}: {error}"))?;
                    // Constrained stages cannot think (the grammar would take the thinking block).
                    if stage.effort != "none" {
                        return Err(format!("stages.toml: stage {name} has a schema, so its effort must be `none`"));
                    }
                    Some(shape)
                }
                None => None,
            };
            if let Some((_, fields, values)) = SCHEMA_NEEDS.iter().find(|(stage_name, _, _)| stage_name == name) {
                let Some(shape) = &shape else { return Err(format!("stages.toml: stage {name} needs a schema")) };
                for field in *fields {
                    if shape.field(field).is_none() {
                        return Err(format!("stage {name}: the schema must have `{field}`"));
                    }
                }
                if let Some((field, known)) = values
                    && let Some(Shape::OneOf(offered)) = shape.field(field)
                    && let Some(unknown) = offered.iter().find(|value| !known.contains(&value.as_str()))
                {
                    return Err(format!("stage {name}: `{field}` offers `{unknown}`, which the flow cannot act on"));
                }
            }
            stages.insert((*name).to_owned(), StageConfig { rules, shape, effort: stage.effort.clone(), sampling: stage.sampling.clone() });
        }
        for role in ROLES {
            if !raw.protocol.contains_key(*role) {
                return Err(format!("stages.toml: protocol.{role} is missing"));
            }
        }
        for (role, list) in &raw.protocol {
            if !ROLES.contains(&role.as_str()) {
                return Err(format!("stages.toml: protocol names unknown role `{role}`"));
            }
            if let Some(stage) = list.iter().find(|stage| !stages.contains_key(*stage)) {
                return Err(format!("stages.toml: protocol.{role} names unknown stage `{stage}`"));
            }
        }

        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for (name, (text, _)) in files {
            hash = fnv(name.as_bytes(), hash);
            hash = fnv(&[0], hash);
            hash = fnv(text.as_bytes(), hash);
            hash = fnv(&[0], hash);
        }
        Ok(Dataset {
            label: raw.label.clone(),
            hash: format!("{hash:016x}"),
            sources: files.iter().map(|(name, (_, source))| (name.clone(), source.clone())).collect(),
            protocol: raw.protocol.clone(),
            effort: raw.effort.clone(),
            limits: raw.limits.clone(),
            stages,
            sampling,
            texts,
            prompts,
        })
    }

    /// `label@hash` (recorded in run status entries).
    pub fn version(&self) -> String {
        format!("{}@{}", self.label, &self.hash[..12])
    }

    /// A text with its placeholders filled in. Unknown keys render as the key (cannot happen for a
    /// validated set: every shipped key exists).
    pub fn text(&self, key: &str, vars: &[(&str, &str)]) -> String {
        let mut text = self.texts.get(key).cloned().unwrap_or_else(|| key.to_owned());
        for (name, value) in vars {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }

    pub fn prompt(&self, id: &str) -> &str {
        self.prompts.get(id).map(String::as_str).unwrap_or("")
    }

    pub fn stage(&self, name: &str) -> Option<&StageConfig> {
        self.stages.get(name)
    }

    /// The stages whose rules `role`'s system prompt carries.
    pub fn protocol(&self, role: &str) -> &[String] {
        self.protocol.get(role).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether `role`'s system prompt carries `stage`'s rules (otherwise its stage message does).
    pub fn in_prefix(&self, role: &str, stage: &str) -> bool {
        self.protocol(role).iter().any(|name| name == stage)
    }

    /// The prompt ids of a stage's rules, in order.
    pub fn rule_parts(&self, stage: &str) -> &[String] {
        self.stage(stage).map(|config| config.rules.as_slice()).unwrap_or(&[])
    }

    /// The rules of a stage, whole.
    pub fn rules(&self, stage: &str) -> String {
        self.rule_parts(stage).iter().map(|id| self.prompt(id)).collect::<Vec<_>>().join("\n")
    }

    /// The constrained shape of a stage.
    pub fn shape(&self, stage: &str) -> Option<&Shape> {
        self.stage(stage).and_then(|config| config.shape.as_ref())
    }

    /// The effort a stage runs at, given the effort its decision chose.
    pub fn effort_for(&self, stage: &str, decided: &str) -> String {
        match self.stage(stage).map(|config| config.effort.as_str()) {
            Some("decided") | None => decided.to_owned(),
            Some(fixed) => fixed.to_owned(),
        }
    }

    /// Sampling for a stage at a thinking setting.
    pub fn sampling(&self, stage: &str, thinking: bool) -> Sampling {
        let profile = match self.stage(stage).map(|config| config.sampling.as_str()) {
            Some("auto") | None => if thinking { "thinking" } else { "plain" },
            Some(name) => name,
        };
        self.sampling.get(profile).cloned().unwrap_or_default()
    }
}

/// The active data set, its layers and its reload watcher.
pub struct Data {
    current: RwLock<Arc<Dataset>>,
    layers: Mutex<Vec<(String, PathBuf)>>,
    signature: Mutex<Vec<(String, u64, SystemTime)>>,
    last_error: Mutex<Option<String>>,
}

/// Outcome of one (re)load, as published.
#[derive(Debug, Clone, PartialEq)]
pub enum Loaded {
    Unchanged,
    Activated(String),
    Rejected(String),
}

impl Data {
    /// Start from the shipped set.
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            current: RwLock::new(Arc::new(Dataset::embedded()?)),
            layers: Mutex::new(Vec::new()),
            signature: Mutex::new(Vec::new()),
            last_error: Mutex::new(None),
        })
    }

    pub fn get(&self) -> Arc<Dataset> {
        match self.current.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|error| error.clone())
    }

    /// Set the directory layers (lowest first) and load.
    pub fn set_layers(&self, layers: Vec<(String, PathBuf)>) -> Loaded {
        if let Ok(mut current) = self.layers.lock() {
            *current = layers;
        }
        self.reload(true)
    }

    fn signature_now(&self) -> Vec<(String, u64, SystemTime)> {
        let layers = self.layers.lock().map(|layers| layers.clone()).unwrap_or_default();
        let mut signature = Vec::new();
        for (label, dir) in &layers {
            for (name, path) in layer_files(label, dir) {
                if let Ok(meta) = std::fs::metadata(&path) {
                    signature.push((format!("{}:{name}", dir.display()), meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
                }
            }
        }
        signature
    }

    /// Reload when the files changed (or always with `force`).
    pub fn reload(&self, force: bool) -> Loaded {
        let signature = self.signature_now();
        if let Ok(mut known) = self.signature.lock() {
            if !force && *known == signature {
                return Loaded::Unchanged;
            }
            *known = signature;
        }
        let layers = self.layers.lock().map(|layers| layers.clone()).unwrap_or_default();
        let borrowed: Vec<(&str, &Path)> = layers.iter().map(|(label, dir)| (label.as_str(), dir.as_path())).collect();
        let built = gather(&borrowed).and_then(|files| Dataset::build(&files));
        match built {
            Ok(set) => {
                if let Ok(mut error) = self.last_error.lock() {
                    *error = None;
                }
                if !force && set.hash == self.get().hash {
                    return Loaded::Unchanged;
                }
                let version = set.version();
                if let Ok(mut current) = self.current.write() {
                    *current = Arc::new(set);
                }
                Loaded::Activated(version)
            }
            Err(error) => {
                if let Ok(mut last) = self.last_error.lock() {
                    *last = Some(error.clone());
                }
                Loaded::Rejected(error)
            }
        }
    }

    /// The event describing a load outcome.
    pub fn event(&self, loaded: &Loaded) -> Option<Value> {
        let active = self.get();
        match loaded {
            Loaded::Unchanged => None,
            Loaded::Activated(version) => Some(json!({"ok": true, "version": version, "sources": active.sources})),
            Loaded::Rejected(error) => Some(json!({"ok": false, "error": error, "version": active.version()})),
        }
    }
}

/// Poll interval of the data watcher (file metadata only).
pub const WATCH_EVERY: Duration = Duration::from_millis(500);
