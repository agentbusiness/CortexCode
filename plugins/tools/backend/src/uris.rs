//! URI reads: the code graph, tasks, agents, the board, rules and offloaded artifacts become things
//! the model reads. Graph, task, board and rule URIs are answered by the memory plugin, agent URIs
//! by the agents plugin (falling back to memory's agent records) plus the frame's journal.
//! Code locations are rendered as hashlines from the workspace files, so they are editable anchors.

use crate::host::Host;
use crate::util;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{
    AgentRecord, BoardListResult, EdgesResult, ImpactResult, OutlineResult, PathResult, RuleListResult, SearchResult, SymbolMatches,
    SymbolRef, Task, TaskListResult, KnowledgeItem, KnowledgeLinkedResult, KnowledgeListResult};
use cc_protocol::hashline;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub struct Scope<'a> {
    pub project_id: &'a str,
    pub workspace: &'a Path,
}

#[derive(Debug)]
pub struct UriError {
    pub kind: &'static str,
    pub message: String,
}

impl UriError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// The code graph's own name in [`rpc`]: when it is down, the model searches the files instead.
const CODE_GRAPH: &str = "the code graph";

fn rpc(error: RpcError, what: &str) -> UriError {
    match error.code.as_str() {
        cc_protocol::codes::UNAVAILABLE | cc_protocol::codes::UNKNOWN_METHOD | cc_protocol::codes::CONNECTION_CLOSED => {
            if what == CODE_GRAPH {
                UriError::new("unavailable", "the code graph is not available right now; search with run `grep -rn <words>` and read the files.")
            } else {
                UriError::new("unavailable", format!("{what} is not available right now."))
            }
        }
        cc_protocol::codes::NOT_FOUND => UriError::new("not_found", error.message),
        _ => UriError::new("uri_error", format!("{what}: {}", error.message)),
    }
}

pub fn decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text).decode_utf8_lossy().into_owned()
}

/// Split `scheme://rest?query`.
pub fn split_uri(uri: &str) -> Option<(&str, &str, HashMap<String, String>)> {
    let (scheme, rest) = uri.split_once("://")?;
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, query),
        None => (rest, ""),
    };
    let mut params = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(decode(key), decode(value));
    }
    Some((scheme, path, params))
}

pub fn is_uri(target: &str) -> bool {
    split_uri(target).is_some()
}

fn rel(scope: &Scope, path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() { util::display_rel(path, scope.workspace) } else { path.to_string_lossy().into_owned() }
}

fn abs(scope: &Scope, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() { path.to_path_buf() } else { scope.workspace.join(path) }
}

/// Hashline-render lines `start..=end` (1-based) of a workspace file, if it is readable text.
/// One line of a file as plain text (no anchor): result lines already say `path:line`.
fn source_line(scope: &Scope, path: &str, line: u32) -> Option<String> {
    let text = std::fs::read_to_string(abs(scope, path)).ok()?;
    text.lines().nth((line as usize).checked_sub(1)?).map(|line| line.trim_end().to_owned())
}

fn file_lines(scope: &Scope, path: &str, start: u32, end: u32) -> Option<String> {
    let text = std::fs::read_to_string(abs(scope, path)).ok()?;
    let doc = crate::doc::Doc::parse(&text);
    if start == 0 || start as usize > doc.len() {
        return None;
    }
    Some(doc.render(start as usize, end.max(start) as usize))
}

/// One code result as the model reads it: `path:line  signature — description` (the signature's
/// first line, else the qualified name; the description when the indexer wrote one). Nothing else:
/// a small model reads every extra token as something to act on.
fn symbol_line(scope: &Scope, symbol: &SymbolRef) -> String {
    let head = symbol.signature.lines().next().map(str::trim).filter(|line| !line.is_empty()).unwrap_or(&symbol.qname);
    let mut line = format!("{}:{}  {}", rel(scope, &symbol.path), symbol.start_line, head);
    if let Some(description) = symbol.description.as_deref().and_then(|text| text.lines().next()).map(str::trim).filter(|text| !text.is_empty()) {
        let _ = write!(line, " — {description}");
    }
    line
}

/// A symbol's own line in `symbol://`: `path:line  qualified name — description` (its code follows,
/// signature included).
fn symbol_head(scope: &Scope, symbol: &SymbolRef) -> String {
    let mut line = format!("{}:{}  {}", rel(scope, &symbol.path), symbol.start_line, symbol.qname);
    if let Some(description) = symbol.description.as_deref().and_then(|text| text.lines().next()).map(str::trim).filter(|text| !text.is_empty()) {
        let _ = write!(line, " — {description}");
    }
    line
}

/// How many symbols `name` resolves to, and the first one's id ([`resolve_sid`]'s lookup).
async fn resolves_to(host: &dyn Host, scope: &Scope<'_>, name: &str) -> Option<(usize, String)> {
    let value = host.call("memory.symbol", merge(scope_params(scope), json!({ "name": name }))).await.ok()?;
    let found = serde_json::from_value::<SymbolMatches>(value).ok()?;
    Some((found.matches.len(), found.matches.first().map(|detail| detail.symbol.sid.clone()).unwrap_or_default()))
}

/// The name a follow-up URI uses for `symbol`: one that resolves to exactly this symbol. Its short
/// name when that is unique, else its qualified name, else its id.
async fn follow_name(host: &dyn Host, scope: &Scope<'_>, symbol: &SymbolRef) -> String {
    for name in [&symbol.name, &symbol.qname] {
        if !name.is_empty() && resolves_to(host, scope, name).await == Some((1, symbol.sid.clone())) {
            return name.clone();
        }
    }
    symbol.sid.clone()
}

/// A symbol named by the model (its name or qualified name, or an id from earlier tools) as the
/// graph's id. Deterministic: one symbol of that name is that symbol; several are named back (by
/// qualified name, `Err` holds the answer to show) so the model asks again precisely; none leaves
/// the text as given (it may be an id).
async fn resolve_sid(host: &dyn Host, scope: &Scope<'_>, given: &str) -> Result<String, String> {
    let request = merge(scope_params(scope), json!({ "name": given }));
    let Ok(value) = host.call("memory.symbol", request).await else { return Ok(given.to_owned()) };
    let Ok(found) = serde_json::from_value::<SymbolMatches>(value) else { return Ok(given.to_owned()) };
    match found.matches.as_slice() {
        [] => Ok(given.to_owned()),
        [one] => Ok(one.symbol.sid.clone()),
        many => {
            let mut out = format!("Several symbols are named `{given}`; ask again with one of these names:\n");
            for detail in many {
                let _ = writeln!(out, "{}  ({})", symbol_line(scope, &detail.symbol), detail.symbol.qname);
            }
            Err(out)
        }
    }
}

/// The most results one search returns: more do not help a small model; it searches again with
/// other words instead.
const SEARCH_MAX: u64 = 10;

fn scope_params(scope: &Scope) -> Value {
    json!({ "project_id": scope.project_id, "workspace": scope.workspace.to_string_lossy() })
}

fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(base), Value::Object(extra)) = (base.as_object_mut(), extra) {
        for (key, value) in extra {
            base.insert(key, value);
        }
    }
    base
}

fn parse<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T, UriError> {
    serde_json::from_value(value).map_err(|error| UriError::new("uri_error", format!("{what} returned an unexpected shape: {error}")))
}

fn number(params: &HashMap<String, String>, key: &str) -> Option<u64> {
    params.get(key).and_then(|value| value.parse().ok())
}

/// Read one URI. `artifact://` is handled by the caller (it is local).
pub async fn read(host: &dyn Host, scope: &Scope<'_>, uri: &str) -> Result<String, UriError> {
    let Some((scheme, path, params)) = split_uri(uri) else {
        return Err(UriError::new("bad_uri", format!("`{uri}` is not a URI")));
    };
    match scheme {
        "graph" => graph(host, scope, path, &params).await,
        "symbol" => symbol(host, scope, &decode(path)).await,
        "outline" => outline(host, scope, &decode(path)).await,
        "task" => task(host, scope, &decode(path), &params).await,
        "agent" => agent(host, scope, &decode(path)).await,
        "board" => board(host, scope, &params).await,
        "rule" | "rules" => rules(host, scope).await,
        // Project knowledge (docs/PROJECT-KNOWLEDGE-PLAN.md P2): one item by id, or the live list.
        "knowledge" => knowledge(host, scope, &decode(path)).await,
        // The language server of the file's language plugin.
        "skill" => crate::langs::read_skill(host, &decode(path)).await.map_err(|message| UriError::new("not_found", message)),
        "definition" | "references" | "hover" | "diagnostics" => {
            crate::langs::read_uri(host, scope.workspace, scheme, &decode(path)).await.map_err(|message| UriError::new("unavailable", message))
        }
        "research" => crate::research::read(host, uri, scope.workspace.join(crate::sandbox::OFFLINE_MARKER).exists()).await,
        other => Err(UriError::new(
            "bad_uri",
            format!(
                "unknown URI scheme `{other}://`; read takes graph://, symbol://, outline://, definition://, references://, hover://, diagnostics://, artifact://, task://, agent://, board://, rule://, knowledge://, skill:// and research:// (see read's `path`)"
            ),
        )),
    }
}

async fn graph(host: &dyn Host, scope: &Scope<'_>, path: &str, params: &HashMap<String, String>) -> Result<String, UriError> {
    let (verb, rest) = path.split_once('/').unwrap_or((path, ""));
    let depth = number(params, "depth");
    match verb {
        "search" => {
            let text = decode(rest);
            if text.trim().is_empty() {
                return Err(UriError::new("bad_uri", "graph://search/<query> needs a query"));
            }
            let limit = number(params, "limit").unwrap_or(SEARCH_MAX).clamp(1, SEARCH_MAX);
            let mut request = merge(scope_params(scope), json!({ "text": text, "limit": limit }));
            if let Some(kinds) = params.get("kinds") {
                request["kinds"] = json!(kinds.split(',').map(str::trim).collect::<Vec<_>>());
            }
            let result: SearchResult =
                parse(host.call("memory.search", request).await.map_err(|e| rpc(e, CODE_GRAPH))?, "memory.search")?;
            let mut out = String::new();
            for hit in &result.hits {
                let _ = writeln!(out, "{}", symbol_line(scope, &hit.symbol));
            }
            if result.hits.is_empty() {
                out.push_str("Nothing found. Search again with other words, or run `grep -rn` for exact text.\n");
            }
            Ok(out)
        }
        // `used-by` / `uses` are the names the model is shown (`callers` / `callees` differ by two
        // letters and point opposite ways); the old names stay accepted.
        "used-by" | "uses" | "callers" | "callees" => {
            let verb = if matches!(verb, "used-by" | "callers") { "callers" } else { "callees" };
            let given = decode(rest);
            if given.is_empty() {
                return Err(UriError::new("bad_uri", "graph://used-by/<name> and graph://uses/<name> need a function or method name"));
            }
            let sid = match resolve_sid(host, scope, &given).await {
                Ok(sid) => sid,
                Err(choices) => return Ok(choices),
            };
            let method = if verb == "callers" { "memory.callers" } else { "memory.callees" };
            let request = merge(scope_params(scope), json!({ "sid": sid, "depth": depth.unwrap_or(1) }));
            let result: EdgesResult = parse(host.call(method, request).await.map_err(|e| rpc(e, CODE_GRAPH))?, method)?;
            // One line per call site: where it is, which function uses it (or which one it uses),
            // and the calling line itself.
            let mut out = String::new();
            for site in &result.sites {
                let (word, other) = if verb == "callers" { ("in", &site.caller) } else { ("calls", &site.callee) };
                let code = source_line(scope, &site.path, site.line).map(|line| line.trim().to_owned()).unwrap_or_default();
                let _ = writeln!(out, "{}:{}  {word} {}: {}", rel(scope, &site.path), site.line, other.qname, code);
            }
            if result.sites.is_empty() {
                out.push_str(if verb == "callers" { "Nothing in this repository uses it.\n" } else { "It uses nothing found in this repository.\n" });
            }
            Ok(out)
        }
        "impact" => {
            let given = decode(rest);
            let sid = if given.is_empty() {
                given
            } else {
                match resolve_sid(host, scope, &given).await {
                    Ok(sid) => sid,
                    Err(choices) => return Ok(choices),
                }
            };
            let mut request = merge(scope_params(scope), json!({ "depth": depth.unwrap_or(3) }));
            if let Some(paths) = params.get("paths") {
                request["paths"] = json!(paths.split(',').collect::<Vec<_>>());
                request["sids"] = json!([]);
            } else {
                if sid.is_empty() {
                    return Err(UriError::new("bad_uri", "graph://impact/<name> needs a function or method name (or ?paths=a.rs,b.rs)"));
                }
                request["sids"] = json!([sid]);
            }
            let result: ImpactResult =
                parse(host.call("memory.impact", request).await.map_err(|e| rpc(e, CODE_GRAPH))?, "memory.impact")?;
            // What a change here reaches: the places that call it, then the tests that cover it.
            let mut out = String::new();
            if !result.call_sites.is_empty() {
                out.push_str("Called from:\n");
                for site in &result.call_sites {
                    let code = source_line(scope, &site.path, site.line).map(|line| line.trim().to_owned()).unwrap_or_default();
                    let _ = writeln!(out, "{}:{}  in {}: {}", rel(scope, &site.path), site.line, site.caller.qname, code);
                }
            }
            if !result.tests.is_empty() {
                out.push_str("Tests:\n");
                for test in &result.tests {
                    let _ = writeln!(out, "{}", symbol_line(scope, test));
                }
            }
            if out.is_empty() {
                out.push_str("Nothing in this repository calls or tests it.\n");
            }
            // Tasks and rules on it by what they say (ids alone tell the model nothing).
            let titles = task_titles(host, &result.tasks).await;
            if !titles.is_empty() {
                let _ = writeln!(out, "Tasks on it: {}", titles.join("; "));
            }
            let rules = rule_texts(host, scope, &result.rules).await;
            if !rules.is_empty() {
                out.push_str("Rules on it:\n");
                for rule in &rules {
                    let _ = writeln!(out, "- {rule}");
                }
            }
            Ok(out)
        }
        "path" => {
            let (from, to) = split_pair(host, scope, rest).await?;
            let request =
                merge(scope_params(scope), json!({ "from": from, "to": to, "max_hops": number(params, "max_hops").unwrap_or(8) }));
            let result: PathResult = parse(host.call("memory.path", request).await.map_err(|e| rpc(e, CODE_GRAPH))?, "memory.path")?;
            if !result.found {
                return Ok(format!("No connection found between {from} and {to}. See what is near each: graph://used-by/{from}, graph://uses/{to}.\n"));
            }
            // The shortest chain, one step per line, with how each step reaches the next.
            let mut out = String::new();
            for step in &result.steps {
                let _ = writeln!(out, "{}", symbol_line(scope, &step.symbol));
                if let Some(via) = &step.via {
                    let _ = writeln!(out, "  {}", via.to_lowercase().replace('_', " "));
                }
            }
            Ok(out)
        }
        "" => {
            Err(UriError::new("bad_uri", "graph:// needs a query: search/<words>, used-by/<name>, uses/<name>, impact/<name>, path/<name>/<name>"))
        }
        other => Err(UriError::new("bad_uri", format!("unknown graph query `{other}`; use search, used-by, uses, impact or path"))),
    }
}

/// `<a>/<b>` where symbol ids may contain `/`: a unique split is used directly; otherwise each split
/// point is tried and the one where both sides resolve to symbols wins (an ambiguity is an error).
async fn split_pair(host: &dyn Host, scope: &Scope<'_>, rest: &str) -> Result<(String, String), UriError> {
    let positions: Vec<usize> = rest.match_indices('/').map(|(index, _)| index).collect();
    match positions.len() {
        0 => Err(UriError::new("bad_uri", "graph://path/<a>/<b> needs two symbols")),
        1 => Ok((decode(&rest[..positions[0]]), decode(&rest[positions[0] + 1..]))),
        _ => {
            let mut valid = Vec::new();
            for position in positions {
                let (a, b) = (decode(&rest[..position]), decode(&rest[position + 1..]));
                if a.is_empty() || b.is_empty() {
                    continue;
                }
                let resolves = |name: String| {
                    let request = merge(scope_params(scope), json!({ "name": name }));
                    async move {
                        host.call("memory.symbol", request)
                            .await
                            .ok()
                            .and_then(|value| serde_json::from_value::<SymbolMatches>(value).ok())
                            .is_some_and(|matches| !matches.matches.is_empty())
                    }
                };
                if resolves(a.clone()).await && resolves(b.clone()).await {
                    valid.push((a, b));
                }
            }
            match valid.len() {
                1 => Ok(valid.remove(0)),
                0 => Err(UriError::new(
                    "not_found",
                    format!("graph://path/{rest}: no split into two known symbols; percent-encode `/` inside ids as %2F"),
                )),
                _ => Err(UriError::new(
                    "bad_uri",
                    format!("graph://path/{rest} splits into symbols in several ways; percent-encode `/` inside ids as %2F"),
                )),
            }
        }
    }
}

async fn symbol(host: &dyn Host, scope: &Scope<'_>, name: &str) -> Result<String, UriError> {
    if name.trim().is_empty() {
        return Err(UriError::new("bad_uri", "symbol://<name> needs a name, qualified name or sid"));
    }
    let request = merge(scope_params(scope), json!({ "name": name }));
    let result: SymbolMatches = parse(host.call("memory.symbol", request).await.map_err(|e| rpc(e, CODE_GRAPH))?, "memory.symbol")?;
    if result.matches.is_empty() {
        return Err(UriError::new("not_found", format!("symbol://{name}: no such symbol (try graph://search/{name})")));
    }
    // Each match: its line, what it takes and returns, where it goes next (by name), its code.
    let mut out = String::new();
    for detail in &result.matches {
        let symbol = &detail.symbol;
        let _ = writeln!(out, "{}", symbol_head(scope, symbol));
        if !detail.params.is_empty() {
            let params: Vec<String> = detail
                .params
                .iter()
                .map(|param| {
                    let mut text = param.name.clone();
                    if let Some(type_text) = &param.type_text {
                        let _ = write!(text, ": {type_text}");
                    }
                    if let Some(default) = &param.default_value {
                        let _ = write!(text, " = {default}");
                    }
                    text
                })
                .collect();
            let _ = writeln!(out, "Takes: {}", params.join(", "));
        }
        if let Some(returns) = &detail.returns {
            let _ = writeln!(out, "Returns: {returns}");
        }
        if detail.callers > 0 || detail.callees > 0 {
            let name = follow_name(host, scope, symbol).await;
            if detail.callers > 0 {
                let _ = writeln!(out, "Used in {} place(s): graph://used-by/{name}", detail.callers);
            }
            if detail.callees > 0 {
                let _ = writeln!(out, "Uses {} other(s): graph://uses/{name}", detail.callees);
            }
        }
        if !detail.tests.is_empty() {
            let tests: Vec<String> = detail.tests.iter().map(|test| test.qname.clone()).collect();
            let _ = writeln!(out, "Tested by: {}", tests.join(", "));
        }
        match file_lines(scope, &symbol.path, symbol.start_line, symbol.end_line) {
            Some(code) => out.push_str(&code),
            None => {
                if let Some(code) = &detail.code {
                    let _ = writeln!(out, "{code}");
                }
            }
        }
        out.push('\n');
    }
    let sids: Vec<String> = result.matches.iter().map(|detail| detail.symbol.sid.clone()).collect();
    for line in knowledge_lines(host, scope, &sids, &[]).await {
        let _ = writeln!(out, "Known about it: {line}");
    }
    Ok(out)
}

async fn outline(host: &dyn Host, scope: &Scope<'_>, path: &str) -> Result<String, UriError> {
    if path.trim().is_empty() {
        return Err(UriError::new("bad_uri", "outline://<path> needs a file path"));
    }
    let request = merge(scope_params(scope), json!({ "path": path }));
    match host.call("memory.outline", request).await {
        Ok(value) => {
            let result: OutlineResult = parse(value, "memory.outline")?;
            // What is in the file, in order: each class and function's first line at its line
            // number (its indentation shows what it sits inside).
            let mut out = String::new();
            for symbol in &result.symbols {
                let first = source_line(scope, &symbol.path, symbol.start_line).unwrap_or_default();
                let head = if first.trim().is_empty() { symbol.qname.clone() } else { first };
                let _ = writeln!(out, "{}  {}", symbol.start_line, head);
            }
            if out.is_empty() {
                out.push_str("No classes or functions found in this file.\n");
            }
            Ok(out)
        }
        Err(error) if matches!(error.code.as_str(), cc_protocol::codes::UNAVAILABLE | cc_protocol::codes::UNKNOWN_METHOD) => {
            // The structural outline does not need the graph.
            let file = abs(scope, path);
            let text = std::fs::read_to_string(&file).map_err(|e| UriError::new("not_found", format!("{path}: {e}")))?;
            let doc = crate::doc::Doc::parse(&text);
            Ok(crate::read::render_outline(&file, path, &text, &doc, true))
        }
        Err(error) => Err(rpc(error, "the code graph")),
    }
}

fn render_task(out: &mut String, task: &Task) {
    let _ = writeln!(out, "task {}  [{}]  {}", task.id, task.status, task.title);
    if let Some(parent) = &task.parent_id {
        let _ = writeln!(out, "parent: {parent}");
    }
    if let Some(assignee) = &task.assignee {
        let _ = writeln!(out, "assignee: {assignee}");
    }
    if !task.depends_on.is_empty() {
        let _ = writeln!(out, "depends on: {}", task.depends_on.join(", "));
    }
    if !task.brief.is_empty() {
        let _ = writeln!(out, "brief:\n{}", task.brief.trim_end());
    }
    if !task.plan.is_empty() {
        let _ = writeln!(out, "plan:\n{}", task.plan.trim_end());
    }
    if !task.contract.checks.is_empty() {
        let _ = writeln!(out, "contract:");
        for check in &task.contract.checks {
            let _ = writeln!(
                out,
                "- {} ({}): {}{}",
                check.name,
                check.kind,
                check.spec,
                check.expect.as_ref().map(|expect| format!("  expect: {expect}")).unwrap_or_default()
            );
        }
    }
    if !task.touches.is_empty() {
        let _ = writeln!(out, "touches: {}", task.touches.join(", "));
    }
    if let Some(result) = &task.result {
        let _ = writeln!(out, "result:\n{}", result.trim_end());
    }
}

async fn task(host: &dyn Host, scope: &Scope<'_>, id: &str, params: &HashMap<String, String>) -> Result<String, UriError> {
    if id.trim().is_empty() {
        let mut request = json!({ "project_id": scope.project_id });
        if let Some(status) = params.get("status") {
            request["status"] = json!(status);
        }
        let result: TaskListResult = parse(host.call("memory.task.list", request).await.map_err(|e| rpc(e, "tasks"))?, "memory.task.list")?;
        if result.tasks.is_empty() {
            return Ok("No tasks.\n".to_owned());
        }
        let mut out = String::new();
        for task in &result.tasks {
            let _ = writeln!(
                out,
                "- {}  [{}]  {}{}",
                task.id,
                task.status,
                task.title,
                task.assignee.as_ref().map(|a| format!("  @{a}")).unwrap_or_default()
            );
        }
        return Ok(out);
    }
    let task: Task = parse(host.call("memory.task.get", json!({ "id": id })).await.map_err(|e| rpc(e, "tasks"))?, "memory.task.get")?;
    let mut out = String::new();
    render_task(&mut out, &task);
    Ok(out)
}

async fn agent(host: &dyn Host, scope: &Scope<'_>, name: &str) -> Result<String, UriError> {
    let name = name.trim_start_matches('@');
    let request = json!({ "project_id": scope.project_id });
    let agents: Vec<AgentRecord> = match host.call("agents.list", request.clone()).await {
        Ok(value) => parse::<cc_protocol::contracts::agents::ListResult>(value, "agents.list")?.agents,
        Err(_) => {
            parse::<cc_protocol::contracts::memory::AgentListResult>(
                host.call("memory.agent.list", request).await.map_err(|e| rpc(e, "agents"))?,
                "memory.agent.list",
            )?
            .agents
        }
    };
    if name.is_empty() {
        if agents.is_empty() {
            return Ok("No agents.\n".to_owned());
        }
        let mut out = String::new();
        for agent in &agents {
            let _ = writeln!(
                out,
                "- @{}  {}  [{}]{}",
                agent.name,
                agent.role,
                agent.status,
                agent.task_id.as_ref().map(|t| format!("  task {t}")).unwrap_or_default()
            );
        }
        return Ok(out);
    }
    let Some(agent) = agents.iter().find(|agent| agent.name == name) else {
        let names: Vec<String> = agents.iter().map(|agent| format!("@{}", agent.name)).collect();
        return Err(UriError::new(
            "not_found",
            format!("agent://{name}: no such agent (agents: {})", if names.is_empty() { "none".to_owned() } else { names.join(", ") }),
        ));
    };
    let mut out = format!("agent @{}  role {}  [{}]\n", agent.name, agent.role, agent.status);
    if !agent.charter.is_empty() {
        let _ = writeln!(out, "charter: {}", agent.charter);
    }
    if let Some(task) = &agent.task_id {
        let _ = writeln!(out, "task: {task} (task://{task})");
    }
    let _ = writeln!(out, "tasks done: {}  failed: {}", agent.tasks_done, agent.tasks_failed);
    if let Some(session) = &agent.session_id
        && let Ok(value) = host.call("frame.journal.read", json!({ "session_id": session })).await
        && let Ok(journal) = serde_json::from_value::<cc_protocol::contracts::frame::JournalReadResult>(value)
        && !journal.entries.is_empty()
    {
        let start = journal.entries.len().saturating_sub(12);
        out.push_str("recent activity:\n");
        for entry in &journal.entries[start..] {
            let _ = writeln!(out, "- {}: {}", entry.kind, summarize_entry(&entry.kind, &entry.data));
        }
    }
    Ok(out)
}

/// One line per journal entry: the primary text field of each documented entry kind.
fn summarize_entry(kind: &str, data: &Value) -> String {
    let field = |name: &str| data.get(name).and_then(Value::as_str).unwrap_or("").to_owned();
    let text = match kind {
        "tool_call" => format!("{} {}", field("tool"), field("intent")),
        "tool_result" if data.get("ok").and_then(Value::as_bool) == Some(false) => format!("failed: {}", field("output")),
        "tool_result" => field("output"),
        "status" => format!("{} {}", field("state"), field("detail")),
        "error" => field("message"),
        _ => field("text"),
    };
    let first: String = text.lines().next().unwrap_or("").chars().take(160).collect();
    if text.lines().count() > 1 || text.chars().count() > 160 { format!("{first} …") } else { first }
}

async fn board(host: &dyn Host, scope: &Scope<'_>, params: &HashMap<String, String>) -> Result<String, UriError> {
    let mut request = json!({ "project_id": scope.project_id });
    if let Some(since) = number(params, "since_ms") {
        request["since_ms"] = json!(since);
    }
    if let Some(to) = params.get("to") {
        request["to"] = json!(to);
    }
    let result: BoardListResult =
        parse(host.call("memory.board.list", request).await.map_err(|e| rpc(e, "the board"))?, "memory.board.list")?;
    if result.messages.is_empty() {
        return Ok("No messages.\n".to_owned());
    }
    let mut out = String::new();
    for message in &result.messages {
        let _ = writeln!(
            out,
            "- {} {}{}: {}",
            message.kind,
            message.author,
            message.to.as_ref().map(|to| format!(" -> {to}")).unwrap_or_default(),
            message.text.trim_end()
        );
        if !message.refs.is_empty() {
            let _ = writeln!(out, "    refs: {}", message.refs.join(", "));
        }
    }
    Ok(out)
}

/// The knowledge items bound to code (`memory.knowledge.linked`), one rendered line each, in the
/// order of the code asked for; nothing when memory has none or is not there (a read never fails
/// for what rides along with it).
pub async fn knowledge_lines(host: &dyn Host, scope: &Scope<'_>, sids: &[String], paths: &[String]) -> Vec<String> {
    if sids.is_empty() && paths.is_empty() {
        return Vec::new();
    }
    let request = json!({ "project_id": scope.project_id, "sids": sids, "paths": paths });
    match host.call("memory.knowledge.linked", request).await {
        Ok(value) => serde_json::from_value::<KnowledgeLinkedResult>(value).map(|result| result.items.into_iter().map(|item| item.line).collect()).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// One knowledge item as the model reads it in full.
fn knowledge_item(item: &KnowledgeItem) -> String {
    let mut out = format!("knowledge://{} {} {}
name: {}
{}
", item.id, item.kind, item.state, item.name, item.text);
    let _ = writeln!(out, "source: {}{}", item.source, item.as_of.as_deref().map(|d| format!(", as of {d}")).unwrap_or_default());
    if !item.aliases.is_empty() {
        let _ = writeln!(out, "also called: {}", item.aliases.join(", "));
    }
    if !item.evidence.is_empty() {
        let _ = writeln!(out, "evidence: {}", item.evidence.join(", "));
    }
    for link in &item.links {
        let _ = writeln!(out, "{}: {} ({})", link.rel.replace('_', " "), link.name, link.id);
    }
    if let Some(by) = &item.superseded_by {
        let _ = writeln!(out, "superseded by knowledge://{by}");
    }
    if let Some(reason) = &item.reason {
        let _ = writeln!(out, "withdrawn: {reason}");
    }
    out
}

/// `knowledge://<id>`: one item in full; `knowledge://`: every live item of the project, one line
/// each (kind, state, source, text), newest first.
async fn knowledge(host: &dyn Host, scope: &Scope<'_>, path: &str) -> Result<String, UriError> {
    let id = path.trim().trim_matches('/');
    if id.is_empty() {
        let result: KnowledgeListResult = parse(host.call("memory.knowledge.list", json!({ "project_id": scope.project_id })).await.map_err(|e| rpc(e, "project knowledge"))?, "memory.knowledge.list")?;
        if result.items.is_empty() {
            return Ok("No project knowledge yet.\n".to_owned());
        }
        let mut out = String::new();
        for item in &result.items {
            let state = if matches!(item.kind.as_str(), "hypothesis" | "question") { format!(" {}", item.state) } else { String::new() };
            let _ = writeln!(out, "- knowledge://{} {}{} ({}): {}", item.id, item.kind, state, item.source, item.text);
        }
        return Ok(out);
    }
    let value = host.call("memory.knowledge.get", json!({ "project_id": scope.project_id, "id": id })).await.map_err(|e| rpc(e, "project knowledge"))?;
    let item: KnowledgeItem = parse(value, "memory.knowledge.get")?;
    Ok(knowledge_item(&item))
}

async fn rules(host: &dyn Host, scope: &Scope<'_>) -> Result<String, UriError> {
    let mut out = String::new();
    for rule_scope in [scope.project_id, "global"] {
        let result: RuleListResult =
            parse(host.call("memory.rule.list", json!({ "scope": rule_scope })).await.map_err(|e| rpc(e, "rules"))?, "memory.rule.list")?;
        for rule in &result.rules {
            let _ = writeln!(
                out,
                "- {}{}",
                rule.text.trim_end(),
                if rule.applies_to.is_empty() { String::new() } else { format!(" (applies to {})", rule.applies_to.join(", ")) }
            );
        }
    }
    if out.is_empty() {
        out.push_str("No rules.\n");
    }
    Ok(out)
}

/// The titles of tasks by id (those memory knows; none when it is not there).
async fn task_titles(host: &dyn Host, ids: &[String]) -> Vec<String> {
    let mut titles = Vec::new();
    for id in ids {
        if let Ok(value) = host.call("memory.task.get", json!({ "id": id })).await
            && let Ok(task) = serde_json::from_value::<Task>(value)
            && !task.title.trim().is_empty()
        {
            titles.push(task.title.trim().to_owned());
        }
    }
    titles
}

/// The texts of rules by id, from the project's and the global rules.
async fn rule_texts(host: &dyn Host, scope: &Scope<'_>, ids: &[String]) -> Vec<String> {
    if ids.is_empty() {
        return Vec::new();
    }
    let mut texts = Vec::new();
    for rule_scope in [scope.project_id, "global"] {
        if let Ok(value) = host.call("memory.rule.list", json!({ "scope": rule_scope })).await
            && let Ok(result) = serde_json::from_value::<RuleListResult>(value)
        {
            texts.extend(result.rules.into_iter().filter(|rule| ids.contains(&rule.id)).map(|rule| rule.text.trim_end().to_owned()));
        }
    }
    texts
}

/// Validate a hashline-rendered snippet's tags (used by tests of the rendering).
pub fn tags_verify(rendered: &str) -> bool {
    rendered.lines().all(|line| {
        let Some((head, rest)) = line.split_once('|') else { return false };
        hashline::parse_anchor(head).is_some_and(|anchor| hashline::tag(rest).as_bytes() == anchor.tag)
    })
}
