//! Host reads: what the router or decide asked to see, fetched concurrently from the code graph
//! (memory) and the workspace (tools) and journaled as host `tool_call` / `tool_result` entries.
//!
//! The model never chooses read tools here; it names symbols, files and queries, and the host turns
//! them into reads ("the host executes every read"). A symbol read is followed by its
//! callers (and its impact for change routes) as soon as it resolves.

use crate::host::Host;
use crate::journal::Journal;
use cc_protocol::RpcError;
use cc_protocol::contracts::frame::kinds;
use cc_protocol::contracts::memory::{
    CallSite, EdgesResult, ImpactResult, KnowledgeLinkedResult, OutlineResult, RepoMapResult, SearchResult, SymbolMatches, SymbolRef,
    methods as memory_methods,
};
use cc_protocol::contracts::agents::methods as agents_methods;
use cc_protocol::contracts::lang::{self, LanguageInfo};
use cc_protocol::contracts::tools::{ReadResult, methods as tools_methods};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Where reads look: the project, optionally through a workspace overlay.
#[derive(Clone, Debug)]
pub struct Scope {
    pub project_id: String,
    /// Absolute path of the directory the session works in.
    pub workspace: String,
    /// The project root.
    pub root: String,
}

impl Scope {
    /// The memory scope: no overlay when the session works in the project root.
    fn memory(&self) -> Value {
        if self.workspace == self.root {
            json!({"project_id": self.project_id})
        } else {
            json!({"project_id": self.project_id, "workspace": self.workspace})
        }
    }

    fn with(&self, extra: Value) -> Value {
        let mut value = self.memory();
        if let (Some(map), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            for (key, item) in extra {
                map.insert(key.clone(), item.clone());
            }
        }
        value
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadReq {
    pub tool: String,
    pub args: Value,
    pub intent: String,
    /// Follow a resolved symbol with an impact read (change routes).
    pub impact: bool,
}

impl ReadReq {
    pub fn key(&self) -> String {
        format!("{}:{}", self.tool, self.args)
    }
}

/// What a decision asked to read.
#[derive(Clone, Debug, Default)]
pub struct Wanted<'a> {
    pub symbols: &'a [String],
    pub files: &'a [String],
    pub queries: &'a [String],
    pub focus: &'a [String],
    /// The router's statement of the work, always searched semantically on the first round.
    pub work: Option<&'a str>,
    /// Change routes also read impact.
    pub change: bool,
    /// The session the reads are made for: a file read is that model's own read (anchored, and
    /// counted as read, so the model can edit what it was shown without reading it again).
    pub session_id: &'a str,
    /// When the language servers' diagnostics are read too (ROUTE's round of a change): see
    /// [`diagnostics`], and [`workspace_diagnostics`] when no named file is covered.
    pub diagnostics: Option<DiagnosticsFor<'a>>,
}

/// For whom and from which languages host diagnostics reads are made.
#[derive(Clone, Copy, Debug)]
pub struct DiagnosticsFor<'a> {
    pub session_id: &'a str,
    /// The languages of the active language plugins with a server ([`languages`]).
    pub languages: &'a [LanguageInfo],
    /// A workspace-wide read lists at most this many (errors first, the rest counted).
    pub shown: u32,
}

/// The languages of the active language plugins whose server is installed (described once per
/// plugin instance with a server; each call once, never retried: a plugin that does not answer now
/// covers nothing).
/// Every language an active language plugin declares, whether or not its server is installed:
/// what tells a file name (a declared extension or file name) from a symbol name.
pub async fn declared_languages(host: &Host) -> Vec<LanguageInfo> {
    let Ok(listed) = host.ctx().call("frame.plugins.list", json!({})).await else { return Vec::new() };
    let mut found = Vec::new();
    for entry in listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default() {
        let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        if field("contract") != lang::CONTRACT || field("state") != "active" {
            continue;
        }
        let described = host.ctx().call(&format!("{}.{}", field("category"), lang::methods::DESCRIBE), json!({})).await;
        if let Some(description) = described.ok().and_then(|value| serde_json::from_value::<lang::Description>(value).ok()) {
            found.extend(description.languages);
        }
    }
    found
}

pub async fn languages(host: &Host) -> Vec<LanguageInfo> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<LanguageInfo>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(listed) = host.ctx().call("frame.plugins.list", json!({})).await else { return Vec::new() };
    let mut found = Vec::new();
    for entry in listed.get("plugins").and_then(Value::as_array).cloned().unwrap_or_default() {
        let field = |name: &str| entry.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
        if field("contract") != lang::CONTRACT || field("state") != "active" {
            continue;
        }
        let instance = field("instance");
        let cached = cache.lock().ok().and_then(|cache| cache.get(&instance).cloned());
        let languages = match cached {
            Some(languages) => languages,
            None => {
                let described = host.ctx().call(&format!("{}.{}", field("category"), lang::methods::DESCRIBE), json!({})).await;
                let Some(description) = described.ok().and_then(|value| serde_json::from_value::<lang::Description>(value).ok()) else { continue };
                // Asked again while its server is not installed (an install keeps the instance).
                if !description.server.is_some_and(|server| server.installed) {
                    continue;
                }
                if let Ok(mut cache) = cache.lock() {
                    cache.insert(instance, description.languages.clone());
                }
                description.languages
            }
        };
        found.extend(languages);
    }
    found
}

/// A host `diagnostics://` read for the session: no wait (what the servers have now, marked
/// partial when they had not finished), so the turn never waits for a language server. The skill
/// of a diagnostic code it shows comes with it the way the model's own read would bring it, once
/// per session (the tools' memory: a later model read gets the pointer).
fn diagnostics_read(scope: &Scope, session_id: &str, path: &str, extra: Value, intent: String) -> ReadReq {
    let mut args = json!({"workspace": scope.workspace, "project_id": scope.project_id, "target": format!("diagnostics://{path}"), "session_id": session_id, "wait_ms": 0});
    if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
        args.extend(extra.clone());
    }
    ReadReq { tool: tools_methods::READ.into(), args, intent, impact: false }
}

/// `diagnostics://<file>` for each of `items` that names an existing file of the workspace (a
/// relative path) one of `languages` covers (by name: file names, patterns, extensions), each once,
/// in order.
pub fn diagnostics(scope: &Scope, session_id: &str, items: &[&[String]], languages: &[LanguageInfo]) -> Vec<ReadReq> {
    let mut seen = HashSet::new();
    items
        .iter()
        .flat_map(|list| list.iter())
        .map(|item| item.trim())
        .filter(|item| {
            let path = std::path::Path::new(item);
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            !item.is_empty()
                && path.is_relative()
                && languages.iter().any(|language| language.covers(&name))
                && std::path::Path::new(&scope.workspace).join(path).is_file()
                && seen.insert(item.to_owned())
        })
        .map(|file| diagnostics_read(scope, session_id, file, Value::Null, format!("Diagnostics of {file}")))
        .collect()
}

/// `diagnostics://` (every file the servers reported), at most `shown` listed.
pub fn workspace_diagnostics(scope: &Scope, session_id: &str, shown: u32) -> ReadReq {
    diagnostics_read(scope, session_id, "", json!({"shown": shown}), "Diagnostics of the workspace".into())
}

/// Plan the reads for a decision. Mechanical: each named item maps to its read.
pub fn plan(scope: &Scope, wanted: &Wanted<'_>, limits: &crate::data::Limits) -> Vec<ReadReq> {
    let mut reads = Vec::new();
    let clean = |items: &[String]| -> Vec<String> {
        let mut seen = HashSet::new();
        items.iter().map(|item| item.trim().to_owned()).filter(|item| !item.is_empty() && seen.insert(item.clone())).collect()
    };
    if let Some(work) = wanted.work.map(str::trim).filter(|work| !work.is_empty()) {
        reads.push(ReadReq {
            tool: memory_methods::SEARCH.into(),
            args: scope.with(json!({"text": work, "limit": limits.search_limit})),
            intent: "Search the code graph for the work".into(),
            impact: false,
        });
    }
    for query in clean(wanted.queries) {
        reads.push(ReadReq {
            tool: memory_methods::SEARCH.into(),
            args: scope.with(json!({"text": query, "limit": limits.search_limit})),
            intent: format!("Search for {query}"),
            impact: false,
        });
    }
    for symbol in clean(wanted.symbols) {
        reads.push(ReadReq {
            tool: memory_methods::SYMBOL.into(),
            args: scope.with(json!({"name": symbol})),
            intent: format!("Read symbol {symbol}"),
            impact: wanted.change,
        });
    }
    for file in clean(wanted.files) {
        reads.push(ReadReq {
            tool: tools_methods::READ.into(),
            args: file_read_args(scope, wanted.session_id, &file),
            intent: format!("Read {file}"),
            impact: false,
        });
    }
    if let Some(how) = wanted.diagnostics {
        let named = diagnostics(scope, how.session_id, &[wanted.files, wanted.focus], how.languages);
        // Nothing named that a server checks: what the servers report for the whole workspace,
        // when any installed server could report (none installed: there is nothing to read).
        if named.is_empty() && !how.languages.is_empty() {
            reads.push(workspace_diagnostics(scope, how.session_id, how.shown));
        }
        reads.extend(named);
    }
    let focus = clean(wanted.focus);
    let named_files: HashSet<String> = clean(wanted.files).into_iter().collect();
    for item in &focus {
        // Structural: a focus item that names an existing file gets its outline.
        if !named_files.contains(item) && std::path::Path::new(&scope.workspace).join(item).is_file() {
            reads.push(ReadReq {
                tool: memory_methods::OUTLINE.into(),
                args: scope.with(json!({"path": item})),
                intent: format!("Outline {item}"),
                impact: false,
            });
        }
    }
    if !focus.is_empty() {
        reads.push(ReadReq {
            tool: memory_methods::REPO_MAP.into(),
            args: scope.with(json!({"focus": focus, "budget_tokens": limits.focus_map_tokens})),
            intent: "Repository map around the focus".into(),
            impact: false,
        });
    }
    reads
}

/// A host file read for the session's model (none: an unattributed read of the exact text).
/// A semantic search of the code graph for `text` (shown to the model as `graph://search/<text>`).
pub fn graph_search(scope: &Scope, text: &str, limit: u32) -> ReadReq {
    ReadReq { tool: memory_methods::SEARCH.into(), args: scope.with(json!({"text": text, "limit": limit})), intent: "Search the code graph for the task".into(), impact: false }
}

/// A host read of `file` made as the session's own read (anchored and counted as read).
pub fn file_read(scope: &Scope, session_id: &str, file: &str) -> ReadReq {
    ReadReq { tool: tools_methods::READ.into(), args: file_read_args(scope, session_id, file), intent: format!("Read {file}"), impact: false }
}

fn file_read_args(scope: &Scope, session_id: &str, file: &str) -> Value {
    let mut args = json!({"workspace": scope.workspace, "project_id": scope.project_id, "target": file});
    if !session_id.is_empty() {
        args["session_id"] = json!(session_id);
    }
    args
}

/// What a host read would be as the model's own `read` (its `path`): a file, or the code graph's
/// and the host's URIs. None for a read with no such form (the repository map, the scheduler).
/// The model sees host reads under these names, so a read it asks for and one the host made
/// compare like with like.
pub fn read_target(tool: &str, args: &Value) -> Option<String> {
    let field = |name: &str| args.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    let target = match tool {
        memory_methods::SEARCH => format!("graph://search/{}", field("text")),
        memory_methods::SYMBOL => format!("symbol://{}", field("name")),
        memory_methods::CALLERS => format!("graph://used-by/{}", field("sid")),
        memory_methods::CALLEES => format!("graph://uses/{}", field("sid")),
        memory_methods::OUTLINE => format!("outline://{}", field("path")),
        memory_methods::IMPACT => {
            let list = |name: &str| -> Vec<String> { args.get(name).and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default() };
            let (sids, paths) = (list("sids"), list("paths"));
            if sids.is_empty() {
                format!("graph://impact/?paths={}", paths.join(","))
            } else {
                sids.iter().map(|sid| format!("graph://impact/{sid}")).collect::<Vec<_>>().join(", ")
            }
        }
        tools_methods::READ => field("target"),
        agents_methods::LIST => "agent://".to_owned(),
        memory_methods::TASK_LIST => "task://".to_owned(),
        memory_methods::BOARD_LIST => "board://".to_owned(),
        _ => return None,
    };
    Some(target)
}

struct Done {
    call_id: String,
    ok: bool,
    output: String,
    ms: u64,
    follow: Vec<ReadReq>,
}

/// Run reads concurrently, journaling each call when it starts and its result when it finishes.
/// Reads already made in this run (`done`) are skipped; at most `max` run (follow-ups included).
/// Returns how many reads ran and how many were left out by `max`.
pub async fn run(
    journal: &mut Journal,
    run_id: &str,
    scope: &Scope,
    reads: Vec<ReadReq>,
    done: &mut HashSet<String>,
    cancel: &CancellationToken,
    max: usize,
) -> Result<(usize, usize), RpcError> {
    let host = journal.host().clone();
    let mut pending = FuturesUnordered::new();
    let mut count = 0;
    let mut skipped = 0;
    let mut queue: Vec<ReadReq> = reads;
    loop {
        for req in queue.drain(..) {
            if done.contains(&req.key()) {
                continue;
            }
            if count >= max {
                skipped += 1;
                continue;
            }
            done.insert(req.key());
            let call_id = format!("host-{}", uuid::Uuid::new_v4().simple());
            journal
                .append(
                    kinds::TOOL_CALL,
                    json!({"call_id": call_id, "tool": req.tool, "intent": req.intent, "args": req.args, "host": true, "run_id": run_id}),
                )
                .await?;
            count += 1;
            let host = host.clone();
            let scope = scope.clone();
            pending.push(async move { execute(&host, &scope, req, call_id).await });
        }
        let next = tokio::select! {
            next = pending.next() => next,
            _ = cancel.cancelled() => return Err(RpcError::cancelled()),
        };
        let Some(finished) = next else { break };
        journal
            .append(
                kinds::TOOL_RESULT,
                json!({"call_id": finished.call_id, "ok": finished.ok, "output": finished.output, "duration_ms": finished.ms, "host": true, "run_id": run_id}),
            )
            .await?;
        queue.extend(finished.follow);
    }
    Ok((count, skipped))
}

/// The reads a resolved symbol leads to: its callers, and on a change route the impact of
/// changing it. Structural: a symbol the graph counts no callers of has no call sites to read, and
/// one with neither callers nor tests has no impact to show, so those reads are not made.
fn follow_ups(scope: &Scope, matches: &SymbolMatches, impact: bool) -> Vec<ReadReq> {
    let mut follow = Vec::new();
    for detail in matches.matches.iter().filter(|detail| detail.callers > 0) {
        follow.push(ReadReq {
            tool: memory_methods::CALLERS.into(),
            args: scope.with(json!({"sid": detail.symbol.sid, "depth": 1})),
            intent: format!("Callers of {}", detail.symbol.qname),
            impact: false,
        });
    }
    let sids: Vec<&str> = matches
        .matches
        .iter()
        .filter(|detail| detail.callers > 0 || !detail.tests.is_empty())
        .map(|detail| detail.symbol.sid.as_str())
        .collect();
    if impact && !sids.is_empty() {
        follow.push(ReadReq {
            tool: memory_methods::IMPACT.into(),
            args: scope.with(json!({"sids": sids, "paths": [], "depth": 2})),
            intent: "Impact of changing these symbols".into(),
            impact: false,
        });
    }
    follow
}

async fn execute(host: &Host, scope: &Scope, req: ReadReq, call_id: String) -> Done {
    let started = Instant::now();
    // A read that waits for nothing (`wait_ms` 0) is not retried either: it fails at once.
    let outcome = if req.args.get("wait_ms") == Some(&json!(0)) { host.ctx().call(&req.tool, req.args.clone()).await } else { host.call(&req.tool, req.args.clone()).await };
    let ms = started.elapsed().as_millis() as u64;
    let mut follow = Vec::new();
    let (ok, output) = match outcome {
        Err(error) => (false, format!("The read failed: {}", error.message)),
        Ok(value) => {
            let mut sids = Vec::new();
            let mut paths = Vec::new();
            if req.tool == memory_methods::SYMBOL
                && let Ok(matches) = serde_json::from_value::<SymbolMatches>(value.clone())
            {
                follow = follow_ups(scope, &matches, req.impact);
                sids = matches.matches.iter().map(|detail| detail.symbol.sid.clone()).collect();
            }
            let path_field = if req.tool == memory_methods::OUTLINE { Some("path") } else if req.tool == tools_methods::READ { Some("target") } else { None };
            if let Some(field) = path_field
                && let Some(path) = req.args.get(field).and_then(Value::as_str)
                && !path.contains("://")
            {
                paths.push(path.to_owned());
            }
            let mut output = render(&req.tool, &value);
            // A symbol's code comes from its file on disk, rendered as the model's own
            // `symbol://` read (anchors, repaired index); that read carries its knowledge lines.
            if req.tool == memory_methods::SYMBOL
                && let Some(name) = req.args.get("name").and_then(Value::as_str)
                && let Ok(read) = host.ctx().call(tools_methods::READ, file_read_args(scope, "", &format!("symbol://{name}"))).await
                && let Ok(read) = serde_json::from_value::<ReadResult>(read)
            {
                output = read.text;
                sids.clear();
            }
            // What the project knows about the code served rides along with it, decided by the
            // graph's edges (docs/PROJECT-KNOWLEDGE-PLAN.md P2); nothing when memory has none.
            for line in knowledge_lines(host, scope, &sids, &paths).await {
                output.push_str("\nknowledge: ");
                output.push_str(&line);
            }
            (true, output)
        }
    };
    Done { call_id, ok, output, ms, follow }
}

/// The knowledge items bound to the code a read served (`memory.knowledge.linked`), one rendered
/// line each; a failure or an absent memory plugin yields none, never a failed read.
async fn knowledge_lines(host: &Host, scope: &Scope, sids: &[String], paths: &[String]) -> Vec<String> {
    if sids.is_empty() && paths.is_empty() {
        return Vec::new();
    }
    match host.ctx().call(memory_methods::KNOWLEDGE_LINKED, json!({"project_id": scope.project_id, "sids": sids, "paths": paths})).await {
        Ok(value) => serde_json::from_value::<KnowledgeLinkedResult>(value).map(|result| result.items.into_iter().map(|item| item.line).collect()).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn symbol_line(symbol: &SymbolRef) -> String {
    // The same one-line shape the model's own code reads return: `path:line  signature — what it does`.
    let head = symbol.signature.lines().next().map(str::trim).filter(|line| !line.is_empty()).unwrap_or(&symbol.qname);
    let mut line = format!("{}:{}  {}", symbol.path, symbol.start_line, head);
    if let Some(description) = symbol.description.as_deref().and_then(|text| text.lines().next()).map(str::trim).filter(|text| !text.is_empty()) {
        line.push_str(&format!(" — {description}"));
    }
    line
}

fn site_line(site: &CallSite) -> String {
    format!(
        "{}:{} in `{}` calls `{}`({})",
        site.path,
        site.line,
        site.caller.qname,
        site.callee.name,
        site.args.join(", ")
    )
}

/// Render a read's JSON result as text for the model. Unknown shapes are shown as JSON.
pub fn render(tool: &str, value: &Value) -> String {
    let rendered = match tool {
        memory_methods::SEARCH => serde_json::from_value::<SearchResult>(value.clone()).ok().map(|result| {
            if result.hits.is_empty() {
                return "Nothing found. Search again with other words.".to_owned();
            }
            result.hits.iter().map(|hit| symbol_line(&hit.symbol)).collect::<Vec<_>>().join("\n")
        }),
        memory_methods::SYMBOL => serde_json::from_value::<SymbolMatches>(value.clone()).ok().map(|result| {
            if result.matches.is_empty() {
                return "No symbol by that name.".to_owned();
            }
            result
                .matches
                .iter()
                .map(|detail| {
                    let mut text = symbol_line(&detail.symbol);
                    let short = &detail.symbol.name;
                    if detail.callers > 0 {
                        text.push_str(&format!("\nUsed in {} place(s): graph://used-by/{short}", detail.callers));
                    }
                    if detail.callees > 0 {
                        text.push_str(&format!("\nUses {} other(s): graph://uses/{short}", detail.callees));
                    }
                    if !detail.tests.is_empty() {
                        let tests: Vec<String> = detail.tests.iter().map(|test| test.qname.clone()).collect();
                        text.push_str(&format!("\nTested by: {}", tests.join(", ")));
                    }
                    text
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }),
        memory_methods::CALLERS | memory_methods::CALLEES => {
            serde_json::from_value::<EdgesResult>(value.clone()).ok().map(|result| {
                if result.sites.is_empty() {
                    return "No call sites.".to_owned();
                }
                result.sites.iter().map(site_line).collect::<Vec<_>>().join("\n")
            })
        }
        memory_methods::IMPACT => serde_json::from_value::<ImpactResult>(value.clone()).ok().map(|result| render_impact(&result)),
        memory_methods::OUTLINE => serde_json::from_value::<OutlineResult>(value.clone()).ok().map(|result| {
            let mut text = format!("{} ({})", result.path, result.language);
            for symbol in &result.symbols {
                text.push_str(&format!("\n{}", symbol_line(symbol)));
            }
            text
        }),
        memory_methods::REPO_MAP => serde_json::from_value::<RepoMapResult>(value.clone()).ok().map(|result| result.text),
        tools_methods::READ => serde_json::from_value::<ReadResult>(value.clone()).ok().map(|result| result.text),
        _ => None,
    };
    rendered.unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default())
}

pub fn render_impact(result: &ImpactResult) -> String {
    let mut text = String::new();
    if !result.changed.is_empty() {
        text.push_str("Symbols:");
        for symbol in &result.changed {
            text.push_str(&format!("\n{}", symbol_line(symbol)));
        }
    }
    if !result.call_sites.is_empty() {
        text.push_str("\nCall sites:");
        for site in &result.call_sites {
            text.push_str(&format!("\n{}", site_line(site)));
        }
    }
    if !result.tests.is_empty() {
        text.push_str("\nTests:");
        for test in &result.tests {
            text.push_str(&format!("\n{} ({})", test.qname, test.path));
        }
    }
    if !result.rules.is_empty() {
        text.push_str(&format!("\nRules: {}", result.rules.join("; ")));
    }
    if text.is_empty() { "No impact found.".to_owned() } else { text.trim_start().to_owned() }
}
