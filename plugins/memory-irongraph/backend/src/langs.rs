//! The language plugins (`lang@1`: `lang-rust`, `lang-python`, ...) as the code graph uses them:
//! which plugin covers a file, and the facts it extracts.
//!
//! The active language plugins are found with `frame.plugins.list` (contract `lang@1`, state
//! `active`); each instance is asked `describe` once (a swapped or restarted plugin is a new
//! instance). The list is cached until a `plugin.state` / `plugin.swapped` event about a language
//! plugin invalidates it (and at most [`LIST_TTL`]). A file belongs to the first plugin that names
//! it, by exact file name, then file-name pattern, then extension, then the interpreter of its `#!`
//! line (the order a plugin's own `extract` matches its languages in).
//!
//! Extraction sends a plugin's files to `<category>.extract` in batches bounded by count and bytes;
//! several plugins work at once. A failed call (error, timeout, a reply that does not match the
//! request) never drops a file: the file is indexed as text and carries the failure as its note.

use crate::facts::{FileFacts, Origin};
use crate::host::Host;
use cc_protocol::contracts::{contract_of, lang};
use cc_protocol::{RpcError, codes};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Files per `extract` call.
pub const BATCH_FILES: usize = 200;
/// Source bytes per `extract` call (a larger single file goes alone).
pub const BATCH_BYTES: usize = 8 * 1024 * 1024;
/// How long one `extract` call may take.
pub const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long `describe` and `frame.plugins.list` may take.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(30);
/// The plugin list is listed again after this long even without an event.
pub const LIST_TTL: Duration = Duration::from_secs(30);

/// One language of a plugin, as its `describe` says (`lang@1` `LanguageInfo`; `patterns` is read
/// when a plugin declares it).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Language {
    pub name: String,
    pub sid_prefix: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub filenames: Vec<String>,
    #[serde(default)]
    pub shebangs: Vec<String>,
    /// File-name patterns (`Dockerfile.*`, `*.Dockerfile`; see [`name_matches`]).
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Description {
    #[serde(default)]
    languages: Vec<Language>,
}

/// An active language plugin instance and what it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct LangPlugin {
    pub category: String,
    pub instance: String,
    pub version: String,
    pub languages: Vec<Language>,
}

impl LangPlugin {
    pub fn language(&self, name: &str) -> Option<&Language> {
        self.languages.iter().find(|l| l.name == name)
    }
}

/// The language plugins at one moment.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Active plugins, by category.
    pub plugins: Vec<Arc<LangPlugin>>,
    /// Language plugins listed but not active (category, state).
    pub inactive: Vec<(String, String)>,
    /// What went wrong listing or describing them.
    pub problems: Vec<String>,
    /// Identity of this set of instances (equal keys: the same plugins serve the same files).
    pub key: String,
}

/// The plugin and language a file belongs to.
#[derive(Debug, Clone, Copy)]
pub struct Pick<'a> {
    pub plugin: &'a Arc<LangPlugin>,
    pub language: &'a Language,
}

impl Snapshot {
    fn find(&self, test: impl Fn(&Language) -> bool) -> Option<Pick<'_>> {
        self.plugins.iter().find_map(|plugin| plugin.languages.iter().find(|l| test(l)).map(|language| Pick { plugin, language }))
    }

    /// By exact file name, then name pattern, then extension.
    pub fn by_name(&self, rel: &str) -> Option<Pick<'_>> {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        if let Some(found) = self.find(|l| l.filenames.iter().any(|f| f == name)) {
            return Some(found);
        }
        if let Some(found) = self.find(|l| l.patterns.iter().any(|p| name_matches(p, name))) {
            return Some(found);
        }
        let (_, extension) = name.rsplit_once('.')?;
        self.find(|l| l.extensions.iter().any(|e| e == extension))
    }

    /// By the interpreter a `#!` first line names.
    pub fn by_shebang(&self, text: &str) -> Option<Pick<'_>> {
        let interpreter = shebang_interpreter(text)?;
        self.find(|l| l.shebangs.iter().any(|s| s == interpreter))
    }

    /// The plugin for a file: by name, else by its first line (`head`, when read).
    pub fn pick(&self, rel: &str, head: Option<&str>) -> Option<Pick<'_>> {
        self.by_name(rel).or_else(|| head.and_then(|h| self.by_shebang(h)))
    }

    pub fn has_shebangs(&self) -> bool {
        self.plugins.iter().any(|p| p.languages.iter().any(|l| !l.shebangs.is_empty()))
    }

    pub fn plugin(&self, category: &str) -> Option<&Arc<LangPlugin>> {
        self.plugins.iter().find(|p| p.category == category)
    }
}

/// The interpreter a `#!` line names (`#!/usr/bin/env -S python3 -u` -> `python3`).
pub fn shebang_interpreter(text: &str) -> Option<&str> {
    let line = text.lines().next()?.strip_prefix("#!")?;
    let mut words = line.split_whitespace();
    let program = words.next()?;
    let name = program.rsplit('/').next()?;
    if name == "env" { words.find(|w| !w.starts_with('-') && !w.contains('=')).map(|w| w.rsplit('/').next().unwrap_or(w)) } else { Some(name) }
}

/// Whether the file name `name` matches `pattern` (`lang@1`): `*` stands for any run of
/// characters (none included), every other character for itself, and the whole name must match.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => (0..=t.len()).any(|i| go(&p[1..], &t[i..])),
            Some(c) => t.first() == Some(c) && go(&p[1..], &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = name.chars().collect();
    go(&p, &t)
}

/// A file for a language plugin: its caller-side facts ([`FileFacts::of_text`]), text and plugin.
#[derive(Debug, Clone)]
pub struct Source {
    pub facts: FileFacts,
    pub text: String,
    pub category: String,
    /// The language it was matched as (its symbol-id prefix if the plugin's reply names none).
    pub language: String,
}

impl Source {
    /// `facts` matched to `pick`.
    pub fn new(facts: FileFacts, text: String, pick: Pick<'_>) -> Self {
        Source { facts, text, category: pick.plugin.category.clone(), language: pick.language.name.clone() }
    }
}

#[derive(Default)]
struct State {
    /// `describe` by instance.
    described: HashMap<String, Arc<LangPlugin>>,
    listed: Option<(Arc<Snapshot>, Instant)>,
}

/// The registry of language plugins.
pub struct Languages {
    host: OnceLock<Arc<dyn Host>>,
    state: Mutex<State>,
}

struct CallError {
    message: String,
    /// Worth retrying file by file (the plugin answered, but not for the whole batch).
    per_file: bool,
}

impl Languages {
    /// A registry without a frame (until [`Languages::set_host`]): no file has a plugin.
    pub fn new() -> Arc<Self> {
        Arc::new(Self { host: OnceLock::new(), state: Mutex::new(State::default()) })
    }

    pub fn with_host(host: Arc<dyn Host>) -> Arc<Self> {
        let languages = Self::new();
        languages.set_host(host);
        languages
    }

    /// The frame connection (the first one set stays).
    pub fn set_host(&self, host: Arc<dyn Host>) {
        if self.host.set(host).is_ok() {
            self.invalidate();
        }
    }

    pub fn has_host(&self) -> bool {
        self.host.get().is_some()
    }

    /// The frame connection, for calls to other plugins (`model.count`).
    pub fn host(&self) -> Option<Arc<dyn Host>> {
        self.host.get().cloned()
    }

    /// List the plugins again on the next use (a language plugin changed state or was swapped).
    pub fn invalidate(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.listed = None;
        }
    }

    /// The active language plugins (cached; see the module documentation).
    pub async fn snapshot(&self) -> Arc<Snapshot> {
        if let Ok(state) = self.state.lock()
            && let Some((snapshot, at)) = &state.listed
            && at.elapsed() < LIST_TTL
        {
            return snapshot.clone();
        }
        let Some(host) = self.host.get().cloned() else { return Arc::new(Snapshot::default()) };
        let mut snapshot = Snapshot::default();
        let listed = match tokio::time::timeout(DESCRIBE_TIMEOUT, host.call(cc_protocol::contracts::frame::methods::PLUGINS_LIST, json!({}))).await {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                snapshot.problems.push(format!("frame.plugins.list: {}", error.message));
                return Arc::new(snapshot);
            }
            Err(_) => {
                snapshot.problems.push(format!("frame.plugins.list did not answer within {DESCRIBE_TIMEOUT:?}"));
                return Arc::new(snapshot);
            }
        };
        let entries = listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default();
        for entry in entries {
            let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
            let (category, contract, state, instance, version) = (field("category"), field("contract"), field("state"), field("instance"), field("version"));
            if contract != lang::CONTRACT && contract_of(&category) != Some(lang::CONTRACT) {
                continue;
            }
            if state != "active" {
                snapshot.inactive.push((category, state));
                continue;
            }
            let cached = self.state.lock().ok().and_then(|s| s.described.get(&instance).cloned());
            let plugin = match cached {
                Some(plugin) => plugin,
                None => match describe(&*host, &category, &instance, &version).await {
                    Ok(plugin) => {
                        let plugin = Arc::new(plugin);
                        if let Ok(mut state) = self.state.lock() {
                            state.described.insert(instance.clone(), plugin.clone());
                        }
                        plugin
                    }
                    Err(problem) => {
                        snapshot.problems.push(problem);
                        continue;
                    }
                },
            };
            snapshot.plugins.push(plugin);
        }
        snapshot.plugins.sort_by(|a, b| a.category.cmp(&b.category));
        // One instance serves a category (a swap lists the old and the new one for a moment).
        snapshot.plugins.dedup_by(|a, b| a.category == b.category);
        let active: Vec<String> = snapshot.plugins.iter().map(|p| p.category.clone()).collect();
        snapshot.inactive.retain(|(category, _)| !active.contains(category));
        snapshot.inactive.sort();
        snapshot.inactive.dedup();
        snapshot.key = snapshot.plugins.iter().map(|p| format!("{}@{}", p.category, p.instance)).collect::<Vec<_>>().join(",");
        let snapshot = Arc::new(snapshot);
        if let Ok(mut state) = self.state.lock() {
            let live: Vec<&str> = snapshot.plugins.iter().map(|p| p.instance.as_str()).collect();
            state.described.retain(|instance, _| live.contains(&instance.as_str()));
            state.listed = Some((snapshot.clone(), Instant::now()));
        }
        snapshot
    }

    /// Extract `sources` with their plugins (several plugins at once, batches per plugin). Every
    /// source comes back, in no particular order: with the plugin's facts, or as text with the
    /// failure ([`Origin::Failed`]). `root`: the repository on disk (manifests resolve lock files
    /// and installed dependencies against it).
    pub async fn extract(&self, snapshot: &Snapshot, sources: Vec<Source>, root: Option<&Path>) -> Vec<(FileFacts, String)> {
        self.extract_counted(snapshot, sources, root, None).await
    }

    /// [`Languages::extract`], adding the number of files of each finished batch to `done` (the
    /// progress of a cold index).
    pub async fn extract_counted(&self, snapshot: &Snapshot, sources: Vec<Source>, root: Option<&Path>, done: Option<Arc<AtomicU64>>) -> Vec<(FileFacts, String)> {
        if sources.is_empty() {
            return Vec::new();
        }
        let Some(host) = self.host.get().cloned() else {
            return sources.into_iter().map(|s| failed(s, "no frame connection to reach the language plugins".into())).collect();
        };
        let mut groups: BTreeMap<String, Vec<Source>> = BTreeMap::new();
        for source in sources {
            groups.entry(source.category.clone()).or_default().push(source);
        }
        let root = root.map(|r| r.to_string_lossy().into_owned());
        let mut tasks = tokio::task::JoinSet::new();
        let mut out = Vec::new();
        for (category, files) in groups {
            match snapshot.plugin(&category).cloned() {
                Some(plugin) => {
                    let host = host.clone();
                    let root = root.clone();
                    let done = done.clone();
                    tasks.spawn(async move { extract_plugin(&*host, &plugin, files, root.as_deref(), done.as_deref()).await });
                }
                None => {
                    count(done.as_deref(), files.len());
                    out.extend(files.into_iter().map(|s| failed(s, format!("{category} is not active"))))
                }
            }
        }
        while let Some(done) = tasks.join_next().await {
            match done {
                Ok(files) => out.extend(files),
                Err(error) => tracing::error!(%error, "an extraction task failed"),
            }
        }
        out
    }
}

async fn describe(host: &dyn Host, category: &str, instance: &str, version: &str) -> Result<LangPlugin, String> {
    let method = format!("{category}.{}", lang::methods::DESCRIBE);
    let value = match tokio::time::timeout(DESCRIBE_TIMEOUT, host.call(&method, json!({}))).await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return Err(format!("{method}: {}", error.message)),
        Err(_) => return Err(format!("{method} did not answer within {DESCRIBE_TIMEOUT:?}")),
    };
    let description: Description = serde_json::from_value(value).map_err(|e| format!("{method}: {e}"))?;
    Ok(LangPlugin { category: category.to_owned(), instance: instance.to_owned(), version: version.to_owned(), languages: description.languages })
}

/// Sources in batches of at most [`BATCH_FILES`] files and [`BATCH_BYTES`] bytes.
fn batches(sources: Vec<Source>) -> Vec<Vec<Source>> {
    let mut out = Vec::new();
    let mut current: Vec<Source> = Vec::new();
    let mut bytes = 0usize;
    for source in sources {
        if !current.is_empty() && (current.len() >= BATCH_FILES || bytes + source.text.len() > BATCH_BYTES) {
            out.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes += source.text.len();
        current.push(source);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn count(done: Option<&AtomicU64>, files: usize) {
    if let Some(done) = done {
        done.fetch_add(files as u64, Ordering::Relaxed);
    }
}

async fn extract_plugin(host: &dyn Host, plugin: &LangPlugin, sources: Vec<Source>, root: Option<&str>, done: Option<&AtomicU64>) -> Vec<(FileFacts, String)> {
    let mut out = Vec::with_capacity(sources.len());
    for batch in batches(sources) {
        let files = batch.len();
        match call_extract(host, plugin, &batch, root).await {
            Ok(extracted) => out.extend(batch.into_iter().zip(extracted).map(|(source, facts)| absorbed(plugin, source, facts))),
            Err(error) if error.per_file && batch.len() > 1 => {
                // One file the plugin cannot take must not cost the others their facts.
                for source in batch {
                    let single = std::slice::from_ref(&source);
                    match call_extract(host, plugin, single, root).await {
                        Ok(mut extracted) => out.push(absorbed(plugin, source, extracted.remove(0))),
                        Err(error) => out.push(failed(source, error.message)),
                    }
                }
            }
            Err(error) => out.extend(batch.into_iter().map(|source| failed(source, error.message.clone()))),
        }
        count(done, files);
    }
    out
}

async fn call_extract(host: &dyn Host, plugin: &LangPlugin, batch: &[Source], root: Option<&str>) -> Result<Vec<lang::FileFacts>, CallError> {
    let method = format!("{}.{}", plugin.category, lang::methods::EXTRACT);
    let files: Vec<Value> = batch.iter().map(|s| json!({"path": s.facts.path, "text": s.text})).collect();
    let params = json!({"files": files, "root": root});
    let value = match tokio::time::timeout(EXTRACT_TIMEOUT, host.call(&method, params)).await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return Err(call_error(&method, &error)),
        Err(_) => return Err(CallError { message: format!("{method} did not answer within {EXTRACT_TIMEOUT:?}"), per_file: false }),
    };
    let result: lang::ExtractResult = serde_json::from_value(value).map_err(|e| CallError { message: format!("{method}: unreadable reply: {e}"), per_file: true })?;
    let matches = result.files.len() == batch.len() && result.files.iter().zip(batch).all(|(facts, source)| facts.path == source.facts.path);
    if !matches {
        return Err(CallError { message: format!("{method}: the reply does not match the request ({} facts for {} files)", result.files.len(), batch.len()), per_file: true });
    }
    Ok(result.files)
}

fn call_error(method: &str, error: &RpcError) -> CallError {
    // The plugin is not there (or going away): retrying file by file cannot help.
    let gone = matches!(error.code.as_str(), codes::UNAVAILABLE | codes::DRAINING | codes::CONNECTION_CLOSED | codes::CANCELLED | codes::TIMEOUT);
    CallError { message: format!("{method}: {}", error.message), per_file: !gone }
}

fn absorbed(plugin: &LangPlugin, source: Source, extracted: lang::FileFacts) -> (FileFacts, String) {
    let Source { mut facts, text, language, .. } = source;
    let prefix = plugin.language(&extracted.language).or_else(|| plugin.language(&language)).map(|l| l.sid_prefix.clone()).unwrap_or_else(|| extracted.language.clone());
    facts.absorb(extracted, &prefix, Origin::Plugin { category: plugin.category.clone(), version: plugin.version.clone() });
    (facts, text)
}

fn failed(source: Source, error: String) -> (FileFacts, String) {
    let Source { mut facts, text, category, .. } = source;
    tracing::warn!(path = %facts.path, %category, %error, "language plugin extraction failed; indexed as text");
    facts.origin = Origin::Failed { category, error };
    (facts, text)
}
