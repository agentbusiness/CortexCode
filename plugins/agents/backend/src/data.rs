//! Every model-facing text and tunable of the agents plugin, as data loaded at runtime: the
//! planner's prompts, its JSON Schema, sampling and effort, and the texts agents read (merge
//! hand-backs, board posts). Same layering and rules as the chat flow's data set:
//! 1. the shipped files compiled into the binary;
//! 2. a shipped data directory named by the plugin config (`data_dir`);
//! 3. the live data: `backend/data` in the plugin's folder of the code folder, where the app and
//!    the user edit it (absent in safe mode; it may appear while the plugin runs).
//!
//! A set that fails validation is rejected and the previous one stays active (`agents.data`
//! events report both outcomes); every delegation records the version it planned with.

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

const EMBEDDED: &[(&str, &str)] = &[
    ("stages.toml", include_str!("../data/stages.toml")),
    ("texts.toml", include_str!("../data/texts.toml")),
    ("prompts/planner.md", include_str!("../data/prompts/planner.md")),
    ("prompts/plan.md", include_str!("../data/prompts/plan.md")),
    ("schemas/plan.json", include_str!("../data/schemas/plan.json")),
    ("prompts/review.md", include_str!("../data/prompts/review.md")),
    ("schemas/review.json", include_str!("../data/schemas/review.json")),
];

const EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh"];
/// Fields of each planned task the agents plugin reads.
const TASK_FIELDS: &[&str] = &["title", "brief", "checks", "depends_on"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStages {
    label: String,
    sampling: BTreeMap<String, RawSampling>,
    plan: RawPlan,
    review: RawReview,
    effort: EffortConfig,
    limits: Limits,
    scheduler: SchedulerConfig,
}

/// The scheduler's bounds and control-loop settings (`[scheduler]`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SchedulerConfig {
    pub start: u32,
    pub min: u32,
    pub max: u32,
    pub tick_ms: u64,
    pub window_ms: u64,
    pub settle_ms: u64,
    pub foreground_queue_ms: f64,
    pub foreground_prefill_slowdown: f64,
    pub foreground_tpot_ms: f64,
    pub gain: f64,
    pub probe_after_ms: u64,
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
struct RawPlan {
    policy: String,
    rules: String,
    schema: String,
    effort: String,
    sampling: String,
    attempts: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReview {
    rules: String,
    schema: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffortConfig {
    pub default: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub search_limit: u32,
}

/// One validated data set.
#[derive(Debug, Clone)]
pub struct Dataset {
    pub label: String,
    pub hash: String,
    pub sources: BTreeMap<String, String>,
    pub effort: EffortConfig,
    pub limits: Limits,
    pub scheduler: SchedulerConfig,
    /// Planner attempts after unreadable or invalid plans.
    pub plan_attempts: usize,
    plan_effort: String,
    plan_sampling: String,
    plan_policy: String,
    plan_rules: String,
    plan_shape: Shape,
    review_rules: String,
    review_shape: Shape,
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

/// Data files of a directory layer (only the kinds the set uses; none when the directory does not
/// exist).
fn layer_files(dir: &Path) -> Vec<(String, PathBuf)> {
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
    out.sort();
    out
}

/// The effective files: embedded, then each directory layer over it.
pub fn gather(layers: &[(&str, &Path)]) -> Result<BTreeMap<String, (String, String)>, String> {
    let mut files = embedded_files();
    for (label, dir) in layers {
        for (name, path) in layer_files(dir) {
            let text = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            files.insert(name, (text, (*label).to_owned()));
        }
    }
    Ok(files)
}

impl Dataset {
    pub fn embedded() -> Result<Self, String> {
        Self::build(&embedded_files())
    }

    pub fn build(files: &BTreeMap<String, (String, String)>) -> Result<Self, String> {
        let text_of = |name: &str| files.get(name).map(|(text, _)| text.as_str()).ok_or_else(|| format!("{name} is missing"));
        let raw: RawStages = toml::from_str(text_of("stages.toml")?).map_err(|error| format!("stages.toml: {error}"))?;
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
        let mut sampling = HashMap::new();
        for (name, profile) in &raw.sampling {
            if !(0.0..=2.0).contains(&profile.temperature) || !(0.0..=1.0).contains(&profile.top_p) {
                return Err(format!("stages.toml: sampling `{name}` is out of range"));
            }
            sampling.insert(
                name.clone(),
                Sampling { temperature: profile.temperature, top_p: profile.top_p, top_k: profile.top_k, min_p: profile.min_p, presence_penalty: profile.presence_penalty, repeat_penalty: None, seed: None },
            );
        }
        for needed in ["thinking", "plain"] {
            if !sampling.contains_key(needed) {
                return Err(format!("stages.toml: sampling profile `{needed}` is missing"));
            }
        }
        let plan = &raw.plan;
        for prompt in [&plan.policy, &plan.rules] {
            if !prompts.contains_key(prompt) {
                return Err(format!("stages.toml: [plan] names prompts/{prompt}.md, which does not exist"));
            }
        }
        if plan.effort != "none" {
            return Err("stages.toml: the planner has a schema, so its effort must be `none`".into());
        }
        if plan.sampling != "auto" && !sampling.contains_key(&plan.sampling) {
            return Err(format!("stages.toml: [plan] names unknown sampling `{}`", plan.sampling));
        }
        if plan.attempts == 0 || !EFFORTS.contains(&raw.effort.default.as_str()) || raw.limits.search_limit == 0 || raw.limits.search_limit > 10 {
            return Err("stages.toml: a value is out of range".into());
        }
        let sched = &raw.scheduler;
        if sched.min == 0 || (sched.max != 0 && (sched.min > sched.max || sched.start > sched.max)) || sched.tick_ms == 0 || sched.window_ms == 0 || !(0.0..=10.0).contains(&sched.gain) {
            return Err("stages.toml: [scheduler] values are out of range".into());
        }
        let schema: Value = serde_json::from_str(text_of(&plan.schema)?).map_err(|error| format!("{}: {error}", plan.schema))?;
        let plan_shape = schemas::shape_from_schema(&schema).map_err(|error| format!("{}: {error}", plan.schema))?;
        let Some(Shape::List(task)) = plan_shape.field("tasks") else { return Err(format!("{}: needs a `tasks` array", plan.schema)) };
        for field in TASK_FIELDS {
            if task.field(field).is_none() {
                return Err(format!("{}: each task needs `{field}`", plan.schema));
            }
        }
        let review = &raw.review;
        if !prompts.contains_key(&review.rules) {
            return Err(format!("stages.toml: [review] names prompts/{}.md, which does not exist", review.rules));
        }
        let schema: Value = serde_json::from_str(text_of(&review.schema)?).map_err(|error| format!("{}: {error}", review.schema))?;
        let review_shape = schemas::shape_from_schema(&schema).map_err(|error| format!("{}: {error}", review.schema))?;
        for field in ["reason", "decision"] {
            if review_shape.field(field).is_none() {
                return Err(format!("{}: needs `{field}`", review.schema));
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
            effort: raw.effort.clone(),
            limits: raw.limits.clone(),
            scheduler: raw.scheduler.clone(),
            plan_attempts: plan.attempts,
            plan_effort: plan.effort.clone(),
            plan_sampling: plan.sampling.clone(),
            plan_policy: plan.policy.clone(),
            plan_rules: plan.rules.clone(),
            plan_shape,
            review_rules: review.rules.clone(),
            review_shape,
            sampling,
            texts,
            prompts,
        })
    }

    pub fn version(&self) -> String {
        format!("{}@{}", self.label, &self.hash[..12])
    }

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

    /// The planner's system prompt: its policy and its rules.
    pub fn planner_system(&self) -> String {
        format!("{}\n\n{}", self.prompt(&self.plan_policy), self.prompt(&self.plan_rules))
    }

    /// The reviewer's system prompt: the planner's policy and the review rules.
    pub fn review_system(&self) -> String {
        format!("{}\n\n{}", self.prompt(&self.plan_policy), self.prompt(&self.review_rules))
    }

    pub fn review_shape(&self) -> &Shape {
        &self.review_shape
    }

    pub fn plan_shape(&self) -> &Shape {
        &self.plan_shape
    }

    pub fn plan_effort(&self) -> &str {
        &self.plan_effort
    }

    pub fn plan_sampling(&self, thinking: bool) -> Sampling {
        let profile = match self.plan_sampling.as_str() {
            "auto" => if thinking { "thinking" } else { "plain" },
            name => name,
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
        for (_, dir) in &layers {
            for (name, path) in layer_files(dir) {
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
