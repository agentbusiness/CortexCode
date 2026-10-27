//! memory@1 code queries over one indexed tree.

use crate::attributes::Declared;
use crate::db::{Rows, layer};
use crate::host::Host;
use crate::params;
use crate::tree::CodeTree;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{
    CallSite, EdgesResult, FilesResult, ImpactResult, LineEntry, LinesResult, OutlineResult, ParamInfo, PathResult, PathStep, RepoMapResult, SearchHit,
    SearchResult, SymbolDetail, SymbolMatches, SymbolRef,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `memory.query`: read-only Cypher against a project. This plugin's own inspection method (its
/// tests, verify scripts and `cx call`), not part of `memory@1`: no other plugin depends on Cypher.
pub const QUERY: &str = "memory.query";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryParams {
    pub project_id: String,
    pub cypher: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Relationship types that are "call-like": an invocation site with a location (and usually
/// argument text). `callers` / `callees` / `impact` traverse these.
pub const CALL_LIKE: &[&str] = &["CALLS", "INSTANTIATES", "RENDERS", "USES_HOOK", "REQUESTS", "INVOKES", "HANDLES", "RUNS"];

/// Projection of a node into the columns a [`SymbolRef`] needs.
pub fn sym_cols(var: &str) -> String {
    format!(
        "{v}.sid, {v}.kind, {v}.name, {v}.qname, coalesce({v}.path, {v}.pkg_path), {v}.start_line, {v}.end_line, {v}.signature, {v}.doc, coalesce({v}.language, {v}.ecosystem), coalesce({v}.description, {v}.summary)",
        v = var
    )
}
const SYM_WIDTH: usize = 11;

fn s(row: &[Value], i: usize) -> String {
    row.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned()
}

fn u(row: &[Value], i: usize) -> u32 {
    row.get(i).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0) as u32
}

pub fn sym_from(row: &[Value], at: usize) -> SymbolRef {
    SymbolRef {
        sid: s(row, at),
        kind: s(row, at + 1),
        name: s(row, at + 2),
        qname: s(row, at + 3),
        path: s(row, at + 4),
        start_line: u(row, at + 5),
        end_line: u(row, at + 6),
        signature: s(row, at + 7),
        description: row.get(at + 10).and_then(|v| v.as_str()).filter(|text| !text.is_empty()).map(str::to_owned),
        doc: row.get(at + 8).and_then(|v| v.as_str()).map(str::to_owned),
        language: s(row, at + 9),
    }
}

impl CodeTree {
    /// Resolve a sid, qualified name, path or bare name to symbol sids.
    pub async fn resolve_name(&self, name: &str) -> Result<Vec<SymbolRef>, RpcError> {
        let cols = sym_cols("n");
        let exact = self.run(&format!("{} MATCH (n:Code {{sid: $v}}) RETURN {cols}", layer::OBSERVED_READ), params! { "v" => name }).await?;
        if !exact.rows.is_empty() {
            return Ok(exact.rows.iter().map(|r| sym_from(r, 0)).collect());
        }
        let file = self.run(&format!("{} MATCH (n:File {{sid: $v}}) RETURN {cols}", layer::OBSERVED_READ), params! { "v" => format!("file:{name}") }).await?;
        if !file.rows.is_empty() {
            return Ok(file.rows.iter().map(|r| sym_from(r, 0)).collect());
        }
        let qualified = self.run(&format!("{} MATCH (n:Sym {{qname: $v}}) RETURN {cols}", layer::OBSERVED_READ), params! { "v" => name }).await?;
        if !qualified.rows.is_empty() {
            return Ok(qualified.rows.iter().map(|r| sym_from(r, 0)).collect());
        }
        let bare = self.run(&format!("{} MATCH (n:Sym {{name: $v}}) RETURN {cols}", layer::OBSERVED_READ), params! { "v" => name }).await?;
        let mut out: Vec<SymbolRef> = bare.rows.iter().map(|r| sym_from(r, 0)).collect();
        // `Type::method` / `Class.method` written with the other separator.
        if out.is_empty() && (name.contains("::") || name.contains('.')) {
            let alt = if name.contains("::") { name.replace("::", ".") } else { name.replace('.', "::") };
            let rows = self.run(&format!("{} MATCH (n:Sym {{qname: $v}}) RETURN {cols}", layer::OBSERVED_READ), params! { "v" => alt }).await?;
            out = rows.rows.iter().map(|r| sym_from(r, 0)).collect();
        }
        Ok(out)
    }

    pub async fn search(&self, text: &str, kinds: &[String], limit: u32, include_external: bool) -> Result<SearchResult, RpcError> {
        let limit = limit.max(1) as usize;
        let cols = sym_cols("n");
        let kind_ok = |k: &str| kinds.is_empty() || kinds.iter().any(|x| x == k);
        let mut hits: Vec<SearchHit> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        // Exact name / qualified-name / sid matches first.
        let trimmed = text.trim();
        for (cypher, value) in [
            (format!("{} MATCH (n:Code {{sid: $v}}) RETURN {cols}", layer::OBSERVED_READ), trimmed.to_owned()),
            (format!("{} MATCH (n:Sym {{qname: $v}}) RETURN {cols}", layer::OBSERVED_READ), trimmed.to_owned()),
            (format!("{} MATCH (n:Sym {{name: $v}}) RETURN {cols}", layer::OBSERVED_READ), trimmed.to_owned()),
            (
                if include_external { format!("{} MATCH (n:ExternalSymbol {{name: $v}}) RETURN {cols}", layer::OBSERVED_READ) } else { String::new() },
                trimmed.to_owned(),
            ),
        ] {
            if cypher.is_empty() {
                continue;
            }
            if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
                break;
            }
            let rows = self.run(&cypher, params! { "v" => value }).await?;
            for row in &rows.rows {
                let symbol = sym_from(row, 0);
                if kind_ok(&symbol.kind) && seen.insert(symbol.sid.clone()) {
                    hits.push(SearchHit { symbol, score: 1.0, matched: "name".into() });
                }
            }
        }
        // IronGraph semantic search over the automatic node index, bound to code symbols (their
        // descriptions are on them) and files (bind first, then rank: the corpus is chosen before SEARCH).
        // Document sections (`lang-markdown`, label `Section`, kind `section`) rank with the code.
        let label_filter = if include_external { "n:Sym OR n:File OR n:Section OR n:ExternalSymbol" } else { "n:Sym OR n:File OR n:Section" };
        // A wide pool: vendored code can fill a small one and leave the repository's own out.
        let want = (limit * 8).max(limit + 30);
        let rows = self
            .run(
                &format!(
                    "{} MATCH (n) WHERE {label_filter} WITH n SEARCH n IN (EMBEDDING INDEX semantic_nodes FOR TEXT $text LIMIT $k) SCORE AS score RETURN n.sid, labels(n), score",
                    layer::CODE_READ
                ),
                params! { "text" => text, "k" => want },
            )
            .await?;
        let mut ranked: Vec<(String, f64)> = Vec::new();
        for row in &rows.rows {
            let sid = s(row, 0);
            let score = row.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
            if !ranked.iter().any(|(x, _)| *x == sid) {
                ranked.push((sid, score));
            }
        }
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let sids: Vec<String> = ranked.iter().map(|(sid, _)| sid.clone()).filter(|sid| !seen.contains(sid)).collect();
        if !sids.is_empty() {
            let rows = self.run(&format!("{} UNWIND $sids AS s MATCH (n:Code {{sid: s}}) RETURN {cols}", layer::OBSERVED_READ), params! { "sids" => sids }).await?;
            let by_sid: HashMap<String, SymbolRef> = rows.rows.iter().map(|r| (s(r, 0), sym_from(r, 0))).collect();
            // The repository's own code and documents first, then vendored or generated files
            // ([`Declared`]), each by score: third-party code is what the work reads last.
            let declared = {
                let model = self.model.read().await;
                Declared::read(&self.root, model.files.keys().cloned())
            };
            let mut excluded: HashMap<String, bool> = HashMap::new();
            let (mut own, mut other): (Vec<(&SymbolRef, f64)>, Vec<(&SymbolRef, f64)>) = (Vec::new(), Vec::new());
            for (sid, score) in &ranked {
                let Some(symbol) = by_sid.get(sid).filter(|symbol| kind_ok(&symbol.kind)) else { continue };
                if *excluded.entry(symbol.path.clone()).or_insert_with(|| declared.excluded(&self.root, &symbol.path)) {
                    other.push((symbol, *score));
                } else {
                    own.push((symbol, *score));
                }
            }
            for (symbol, score) in own.into_iter().chain(other) {
                if hits.len() >= limit {
                    break;
                }
                if seen.insert(symbol.sid.clone()) {
                    hits.push(SearchHit { symbol: symbol.clone(), score, matched: "semantic".into() });
                }
            }
        }
        hits.truncate(limit);
        Ok(SearchResult { hits })
    }

    pub async fn symbol(&self, name: &str) -> Result<SymbolMatches, RpcError> {
        let found = self.resolve_name(name).await?;
        let mut matches = Vec::new();
        for symbol in found {
            let sid = symbol.sid.clone();
            let detail = self
                .run(
                    &format!(
                        "{} MATCH (n:Code {{sid: $sid}}) RETURN n.return_type, size([(n)<-[c]-() WHERE type(c) IN $calls | 1]), size([(n)-[c]->() WHERE type(c) IN $calls | 1]), n.param_names, n.param_types, n.param_defaults",
                        layer::OBSERVED_READ
                    ),
                    params! { "sid" => sid, "calls" => CALL_LIKE },
                )
                .await?;
            let row = detail.rows.first().cloned().unwrap_or_default();
            // Parameters are three lists on the definition, in order; "" is no type or default.
            let column = |i: usize| -> Vec<String> { row.get(i).and_then(|v| v.as_array()).map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_owned()).collect()).unwrap_or_default() };
            let (types, defaults) = (column(4), column(5));
            let given = |list: &[String], i: usize| list.get(i).filter(|text| !text.is_empty()).cloned();
            let params: Vec<ParamInfo> = column(3)
                .into_iter()
                .enumerate()
                .map(|(i, name)| ParamInfo { name, position: i as u32, type_text: given(&types, i), default_value: given(&defaults, i) })
                .collect();
            let tests = self.run(&format!("{} MATCH (t:Test)-[:TESTS]->(n:Code {{sid: $sid}}) RETURN {}", layer::OBSERVED_READ, sym_cols("t")), params! { "sid" => sid }).await?;
            let mut tests: Vec<SymbolRef> = tests.rows.iter().map(|r| sym_from(r, 0)).collect();
            tests.sort_by(|a, b| a.sid.cmp(&b.sid));
            tests.dedup_by(|a, b| a.sid == b.sid);
            matches.push(SymbolDetail {
                symbol,
                params,
                returns: row.first().and_then(|v| v.as_str()).map(str::to_owned),
                callers: u(&row, 1),
                callees: u(&row, 2),
                tests,
            });
        }
        Ok(SymbolMatches { matches })
    }

    /// Call sites into (`incoming`) or out of a symbol, breadth-first to `depth`.
    pub async fn call_sites(&self, sid: &str, depth: u32, incoming: bool) -> Result<EdgesResult, RpcError> {
        let start = self.resolve_name(sid).await?;
        let mut frontier: Vec<String> = start.iter().map(|s| s.sid.clone()).collect();
        let mut visited: HashSet<String> = frontier.iter().cloned().collect();
        let mut sites = Vec::new();
        for level in 1..=depth.max(1) {
            if frontier.is_empty() {
                break;
            }
            let cypher = if incoming {
                format!(
                    "{} UNWIND $sids AS s MATCH (a:Code)-[c]->(b:Code {{sid: s}}) WHERE type(c) IN $calls RETURN {}, {}, c.line, c.args, c.props, c.confidence, type(c), c.url, c.channel",
                    layer::OBSERVED_READ,
                    sym_cols("a"),
                    sym_cols("b")
                )
            } else {
                format!(
                    "{} UNWIND $sids AS s MATCH (a:Code {{sid: s}})-[c]->(b:Code) WHERE type(c) IN $calls RETURN {}, {}, c.line, c.args, c.props, c.confidence, type(c), c.url, c.channel",
                    layer::OBSERVED_READ,
                    sym_cols("a"),
                    sym_cols("b")
                )
            };
            let rows = self.run(&cypher, params! { "sids" => frontier, "calls" => CALL_LIKE }).await?;
            let mut next = Vec::new();
            for row in &rows.rows {
                let caller = sym_from(row, 0);
                let callee = sym_from(row, SYM_WIDTH);
                let base = 2 * SYM_WIDTH;
                let line = row.get(base).and_then(|v| v.as_u64()).map(|l| l as u32).unwrap_or(caller.start_line);
                let args: Vec<String> = row
                    .get(base + 1)
                    .and_then(|v| v.as_array())
                    .or_else(|| row.get(base + 2).and_then(|v| v.as_array()))
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                let confidence = row.get(base + 3).and_then(|v| v.as_str()).unwrap_or("heuristic").to_owned();
                let other = if incoming { caller.sid.clone() } else { callee.sid.clone() };
                if visited.insert(other.clone()) {
                    next.push(other);
                }
                sites.push(CallSite { path: caller.path.clone(), caller, callee, line, args, confidence, depth: level });
            }
            frontier = next;
        }
        sites.sort_by(|a, b| (a.depth, &a.path, a.line, &a.callee.sid).cmp(&(b.depth, &b.path, b.line, &b.callee.sid)));
        Ok(EdgesResult { sites })
    }

    pub async fn impact(&self, sids: &[String], paths: &[String], depth: u32) -> Result<ImpactResult, RpcError> {
        let cols = sym_cols("n");
        let mut changed: Vec<SymbolRef> = Vec::new();
        for sid in sids {
            changed.extend(self.resolve_name(sid).await?);
        }
        if !paths.is_empty() {
            let rows = self.run(&format!("{} UNWIND $paths AS p MATCH (n:Code {{path: p}}) WHERE n:Sym RETURN {cols}", layer::OBSERVED_READ), params! { "paths" => paths }).await?;
            changed.extend(rows.rows.iter().map(|r| sym_from(r, 0)));
        }
        changed.sort_by(|a, b| a.sid.cmp(&b.sid));
        changed.dedup_by(|a, b| a.sid == b.sid);
        let mut call_sites = Vec::new();
        let mut affected: HashSet<String> = changed.iter().map(|c| c.sid.clone()).collect();
        for symbol in &changed {
            let result = self.call_sites(&symbol.sid, depth, true).await?;
            for site in result.sites {
                affected.insert(site.caller.sid.clone());
                call_sites.push(site);
            }
        }
        call_sites.sort_by(|a, b| (a.depth, &a.path, a.line, &a.callee.sid, &a.caller.sid).cmp(&(b.depth, &b.path, b.line, &b.callee.sid, &b.caller.sid)));
        call_sites.dedup_by(|a, b| a.caller.sid == b.caller.sid && a.callee.sid == b.callee.sid && a.line == b.line);
        let affected: Vec<String> = affected.into_iter().collect();
        // Tests: tests of anything affected, and affected symbols that are tests.
        let rows = self.run(&format!("{} UNWIND $sids AS s MATCH (n:Test)-[:TESTS]->(x:Code {{sid: s}}) RETURN {cols}", layer::OBSERVED_READ), params! { "sids" => affected }).await?;
        let mut tests: Vec<SymbolRef> = rows.rows.iter().map(|r| sym_from(r, 0)).collect();
        tests.extend(call_sites.iter().filter(|c| c.caller.kind == "test").map(|c| c.caller.clone()));
        tests.sort_by(|a, b| a.sid.cmp(&b.sid));
        tests.dedup_by(|a, b| a.sid == b.sid);
        // Tasks and rules attached to the affected code or its files.
        let mut targets: Vec<String> = affected.clone();
        let files: HashSet<String> = changed.iter().chain(call_sites.iter().map(|c| &c.caller)).map(|s| format!("file:{}", s.path)).collect();
        targets.extend(files);
        let tasks = self.run(&format!("{} UNWIND $sids AS s MATCH (t:Task)-[:TOUCHES]->(x:Code {{sid: s}}) RETURN DISTINCT t.id", layer::ALL_READ), params! { "sids" => targets }).await?;
        let rules = self.run(&format!("{} UNWIND $sids AS s MATCH (r:Rule)-[:CONSTRAINS]->(x:Code {{sid: s}}) RETURN DISTINCT r.id", layer::ALL_READ), params! { "sids" => targets }).await?;
        let mut tasks: Vec<String> = tasks.column(0).filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let mut rules: Vec<String> = rules.column(0).filter_map(|v| v.as_str().map(str::to_owned)).collect();
        tasks.sort();
        tasks.dedup();
        rules.sort();
        rules.dedup();
        Ok(ImpactResult { changed, call_sites, tests, tasks, rules })
    }

    pub async fn path(&self, from: &str, to: &str, max_hops: u32) -> Result<PathResult, RpcError> {
        let a = self.resolve_name(from).await?;
        let b = self.resolve_name(to).await?;
        let mut best: Option<(u64, Vec<String>, Vec<String>)> = None;
        for x in &a {
            for y in &b {
                let rows = self
                    .run(
                        &format!(
                            "{} MATCH (a:Code {{sid: $a}}), (b:Code {{sid: $b}}) CALL graph.shortestpath(a, b) YIELD path, cost RETURN [n IN nodes(path) | n.sid], [r IN relationships(path) | type(r)], cost",
                            layer::OBSERVED_READ
                        ),
                        params! { "a" => x.sid, "b" => y.sid },
                    )
                    .await?;
                if let Some(row) = rows.rows.first() {
                    let cost = row.get(2).and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
                    let nodes: Vec<String> = row.first().and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                    let rels: Vec<String> = row.get(1).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                    if cost <= max_hops as u64 && best.as_ref().is_none_or(|(c, _, _)| cost < *c) {
                        best = Some((cost, nodes, rels));
                    }
                }
            }
        }
        let Some((_, nodes, rels)) = best else { return Ok(PathResult { found: false, steps: Vec::new() }) };
        let rows = self.run(&format!("{} UNWIND $sids AS s MATCH (n:Code {{sid: s}}) RETURN {}", layer::OBSERVED_READ, sym_cols("n")), params! { "sids" => nodes }).await?;
        let by_sid: HashMap<String, SymbolRef> = rows.rows.iter().map(|r| (s(r, 0), sym_from(r, 0))).collect();
        let mut steps = Vec::new();
        for (i, sid) in nodes.iter().enumerate() {
            let symbol = by_sid.get(sid).cloned().unwrap_or(SymbolRef {
                sid: sid.clone(),
                kind: String::new(),
                name: sid.clone(),
                qname: sid.clone(),
                path: String::new(),
                start_line: 0,
                end_line: 0,
                signature: String::new(),
                description: None,
                doc: None,
                language: String::new(),
            });
            steps.push(PathStep { symbol, via: rels.get(i).cloned() });
        }
        Ok(PathResult { found: true, steps })
    }

    pub async fn outline(&self, path: &str) -> Result<OutlineResult, RpcError> {
        let rows = self.run(&format!("{} MATCH (n:Code {{path: $p}}) WHERE n:Sym OR n:File RETURN {}", layer::OBSERVED_READ, sym_cols("n")), params! { "p" => path }).await?;
        if rows.rows.is_empty() {
            return Err(RpcError::not_found(format!("{path} is not indexed")));
        }
        let mut language = String::new();
        let mut symbols: Vec<SymbolRef> = Vec::new();
        for row in &rows.rows {
            let symbol = sym_from(row, 0);
            if symbol.kind == "file" {
                language = symbol.language.clone();
            } else {
                symbols.push(symbol);
            }
        }
        symbols.sort_by(|a, b| (a.start_line, std::cmp::Reverse(a.end_line), &a.qname).cmp(&(b.start_line, std::cmp::Reverse(b.end_line), &b.qname)));
        Ok(OutlineResult { path: path.to_owned(), language, symbols })
    }

    /// Line-level index of a file from the line store.
    pub async fn lines(&self, path: &str, start: Option<u32>, end: Option<u32>) -> Result<LinesResult, RpcError> {
        let hash = {
            let model = self.model.read().await;
            model.files.get(path).map(|f| f.content_hash.clone())
        };
        let Some(hash) = hash else { return Err(RpcError::not_found(format!("{path} is not indexed"))) };
        let index = self.lines.get(&hash).ok_or_else(|| RpcError::not_found(format!("no line index for {path}")))?;
        let total = index.tags.len() as u32;
        let from = start.unwrap_or(1).max(1);
        let to = end.unwrap_or(total).min(total);
        let lines = (from..=to)
            .filter_map(|n| {
                let i = (n - 1) as usize;
                Some(LineEntry { n, tag: index.tags.get(i)?.clone(), offset: *index.offsets.get(i)? })
            })
            .collect();
        Ok(LinesResult { path: path.to_owned(), content_hash: hash, total, lines })
    }

    /// Repository map: PageRank (IronGraph `graph.pagerank`) or, with focus, personalized PageRank
    /// over the code graph, rendered as paths and signatures within `budget_tokens` as the model
    /// counts them ([`MapCounter`]). Files the repository declares vendored or generated
    /// ([`Declared`]) are left out: third-party code the work never edits.
    pub async fn repo_map(&self, focus: &[String], budget_tokens: u32) -> Result<RepoMapResult, RpcError> {
        let scores: Vec<(String, f64)> = if focus.is_empty() { self.pagerank().await? } else { self.personalized_pagerank(focus).await? };
        let sids: Vec<String> = scores.iter().map(|(sid, _)| sid.clone()).collect();
        let rows = self
            .run(&format!("{} UNWIND $sids AS s MATCH (n:Code {{sid: s}}) WHERE n:Sym RETURN {}", layer::OBSERVED_READ, sym_cols("n")), params! { "sids" => sids })
            .await?;
        let by_sid: HashMap<String, SymbolRef> = rows.rows.iter().map(|r| (s(r, 0), sym_from(r, 0))).collect();
        let declared = {
            let model = self.model.read().await;
            Declared::read(&self.root, model.files.keys().cloned())
        };
        // Files ordered by their best symbol; symbols within a file by line.
        let mut file_rank: Vec<String> = Vec::new();
        let mut per_file: HashMap<String, Vec<SymbolRef>> = HashMap::new();
        let mut excluded: HashMap<String, bool> = HashMap::new();
        for (sid, _) in &scores {
            let Some(symbol) = by_sid.get(sid) else { continue };
            if matches!(symbol.kind.as_str(), "prop" | "variant" | "field") {
                continue;
            }
            if *excluded.entry(symbol.path.clone()).or_insert_with(|| declared.excluded(&self.root, &symbol.path)) {
                continue;
            }
            if !per_file.contains_key(&symbol.path) {
                file_rank.push(symbol.path.clone());
            }
            per_file.entry(symbol.path.clone()).or_default().push(symbol.clone());
        }
        // Breadth over files first: each round adds the next-best symbol of every file in rank order.
        let mut order: Vec<(usize, &SymbolRef)> = Vec::new();
        let mut round = 0usize;
        loop {
            let before = order.len();
            for (rank, path) in file_rank.iter().enumerate() {
                if let Some(symbol) = per_file.get(path).and_then(|list| list.get(round)) {
                    order.push((rank, symbol));
                }
            }
            if order.len() == before {
                break;
            }
            round += 1;
        }
        let render = |taken: usize| -> (String, u32, u32) {
            let mut chosen: BTreeMap<usize, Vec<&SymbolRef>> = BTreeMap::new();
            for (rank, symbol) in &order[..taken] {
                chosen.entry(*rank).or_default().push(symbol);
            }
            let mut text = String::new();
            let (mut files, mut symbols) = (0u32, 0u32);
            for (rank, mut list) in chosen {
                list.sort_by_key(|symbol| symbol.start_line);
                text.push_str(&file_rank[rank]);
                text.push_str(":\n");
                files += 1;
                for symbol in list {
                    text.push_str(&format!("  {}: {}\n", symbol.start_line, first_line(&symbol.signature, &symbol.name)));
                    symbols += 1;
                }
            }
            (text, files, symbols)
        };
        // The longest prefix of the order that fits, found by counting: galloping (1, 2, 4, ...
        // symbols) to the first length that overflows, then bisecting between it and the last
        // length that fit.
        let counter = MapCounter::new(self.langs.host()).await;
        let fits = |taken: usize| {
            let text = render(taken).0;
            let counter = &counter;
            async move { counter.tokens(&text).await <= budget_tokens }
        };
        let (mut good, mut bad) = (0usize, order.len() + 1);
        let mut next = 1usize;
        while next <= order.len() {
            if !fits(next).await {
                bad = next;
                break;
            }
            good = next;
            next = if next == order.len() { order.len() + 1 } else { (next * 2).min(order.len()) };
        }
        while bad - good > 1 {
            let mid = good + (bad - good) / 2;
            if fits(mid).await {
                good = mid;
            } else {
                bad = mid;
            }
        }
        let (text, files, symbols) = render(good);
        Ok(RepoMapResult { text, files, symbols })
    }

    /// Global PageRank of symbols (IronGraph's `graph.pagerank` over the OBSERVED code graph).
    pub async fn pagerank(&self) -> Result<Vec<(String, f64)>, RpcError> {
        let rows = self
            .run(&format!("{} CALL graph.pagerank() YIELD node, score WITH node, score WHERE node:Sym RETURN node.sid, score", layer::OBSERVED_READ), Map::new())
            .await?;
        let mut scores: Vec<(String, f64)> = rows.rows.iter().map(|r| (s(r, 0), r.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0))).collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(scores)
    }

    /// Personalized PageRank: power iteration with restarts at the focus nodes (sids, or files whose
    /// definitions seed the walk). IronGraph's procedure has no personalization vector, so the walk
    /// runs here over the edges read from the graph. Mechanical algorithm.
    async fn personalized_pagerank(&self, focus: &[String]) -> Result<Vec<(String, f64)>, RpcError> {
        let rows = self
            .run(
                &format!("{} MATCH (a:Code)-[r]->(b:Code) WHERE type(r) IN $rels RETURN a.sid, b.sid", layer::OBSERVED_READ),
                params! { "rels" => ["CALLS", "INSTANTIATES", "RENDERS", "USES_HOOK", "REQUESTS", "INVOKES", "HANDLES", "RUNS", "IMPORTS", "DEFINES", "EXTENDS", "IMPLEMENTS", "OF_TYPE", "TAKES", "RETURNS", "READS", "WRITES", "TESTS", "CONTAINS"] },
            )
            .await?;
        let mut ids: HashMap<String, usize> = HashMap::new();
        let mut names: Vec<String> = Vec::new();
        let id = |sid: &str, ids: &mut HashMap<String, usize>, names: &mut Vec<String>| -> usize {
            if let Some(i) = ids.get(sid) {
                return *i;
            }
            ids.insert(sid.to_owned(), names.len());
            names.push(sid.to_owned());
            names.len() - 1
        };
        let mut out_edges: Vec<Vec<usize>> = Vec::new();
        for row in &rows.rows {
            let a = id(&s(row, 0), &mut ids, &mut names);
            let b = id(&s(row, 1), &mut ids, &mut names);
            if out_edges.len() < names.len() {
                out_edges.resize(names.len(), Vec::new());
            }
            out_edges[a].push(b);
            // Undirected for relevance: what calls the focus matters as much as what it calls.
            out_edges[b].push(a);
        }
        out_edges.resize(names.len(), Vec::new());
        let n = names.len();
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut seeds: Vec<usize> = Vec::new();
        for f in focus {
            let candidates = [f.clone(), format!("file:{f}")];
            for c in candidates {
                if let Some(i) = ids.get(&c) {
                    seeds.push(*i);
                }
            }
            if !seeds.iter().any(|i| names[*i] == *f || names[*i] == format!("file:{f}")) {
                for symbol in self.resolve_name(f).await? {
                    if let Some(i) = ids.get(&symbol.sid) {
                        seeds.push(*i);
                    }
                }
            }
        }
        if seeds.is_empty() {
            return self.pagerank().await;
        }
        let restart = 1.0 / seeds.len() as f64;
        let damping = 0.85;
        let mut rank = vec![0.0f64; n];
        for s in &seeds {
            rank[*s] += restart;
        }
        for _ in 0..100 {
            let mut next = vec![0.0f64; n];
            let mut dangling = 0.0;
            for (i, edges) in out_edges.iter().enumerate() {
                if edges.is_empty() {
                    dangling += rank[i];
                    continue;
                }
                let share = damping * rank[i] / edges.len() as f64;
                for j in edges {
                    next[*j] += share;
                }
            }
            for s in &seeds {
                next[*s] += (1.0 - damping + damping * dangling) * restart;
            }
            let delta: f64 = next.iter().zip(&rank).map(|(a, b)| (a - b).abs()).sum();
            rank = next;
            if delta < 1e-9 {
                break;
            }
        }
        let mut scores: Vec<(String, f64)> = names.into_iter().zip(rank).collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(scores)
    }

    /// Every indexed file's repository-relative path, sorted.
    pub async fn files(&self) -> Result<FilesResult, RpcError> {
        let rows = self.run(&format!("{} MATCH (f:File) RETURN f.path", layer::OBSERVED_READ), Map::new()).await?;
        let mut paths: Vec<String> = rows.column(0).filter_map(|v| v.as_str().map(str::to_owned)).collect();
        paths.sort();
        paths.dedup();
        Ok(FilesResult { paths })
    }

    /// Read-only Cypher. The engine validates it (`CHECK READ ONLY`); a statement without a layer
    /// selection reads all three layers.
    pub async fn query(&self, cypher: &str, params: Value) -> Result<QueryResult, RpcError> {
        read_only_query(&self.graph, &self.ig_id, cypher, params).await
    }

}

/// Read-only Cypher against one IronGraph project. The engine validates it (`CHECK READ ONLY`);
/// a statement without a layer selection reads all three layers.
pub async fn read_only_query(graph: &crate::db::Graph, ig_id: &str, cypher: &str, params: Value) -> Result<QueryResult, RpcError> {
    let trimmed = cypher.trim();
    let statement = if trimmed.to_uppercase().starts_with("USE LAYER") { trimmed.to_owned() } else { format!("{} {trimmed}", layer::ALL_READ) };
    graph.check_read_only(ig_id, &statement).await?;
    let params = match params {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        _ => return Err(RpcError::invalid_params("params must be an object")),
    };
    let rows: Rows = graph.run(ig_id, &statement, params).await?;
    Ok(QueryResult { columns: rows.columns, rows: rows.rows })
}

impl CodeTree {
    /// Store what a symbol does on its own node: `description`, and `described_hash` (the body it
    /// was written for; a different `body_hash` makes it stale). The indexer never writes these, so
    /// they stay through re-indexing and renames, and go with the symbol when it is deleted.
    pub async fn describe(&self, sid: &str, description: &str) -> Result<SymbolRef, RpcError> {
        let found = self.resolve_name(sid).await?;
        let Some(mut symbol) = found.into_iter().next() else { return Err(RpcError::not_found(format!("no symbol {sid}"))) };
        self.run(
            &format!("{} MATCH (s:Code {{sid: $sid}}) SET s.description = $text, s.described_hash = s.body_hash, s.described_ms = $now", layer::OBSERVED_WRITE),
            params! { "sid" => symbol.sid, "text" => description, "now" => cc_protocol::contracts::now_ms() },
        )
        .await?;
        symbol.description = Some(description.to_owned());
        Ok(symbol)
    }

    /// Definitions without a current description (none, or written for an older body), most
    /// central first (PageRank).
    pub async fn undescribed(&self, limit: u32) -> Result<SearchResult, RpcError> {
        let scores = self.pagerank().await?;
        let described = self.run(&format!("{} MATCH (s:Sym) WHERE s.description IS NOT NULL RETURN s.sid, s.described_hash = s.body_hash", layer::OBSERVED_READ), Map::new()).await?;
        let fresh: HashSet<String> = described.rows.iter().filter(|r| r.get(1).and_then(|v| v.as_bool()) == Some(true)).map(|r| s(r, 0)).collect();
        let candidates: Vec<(String, f64)> = scores.into_iter().filter(|(sid, _)| !fresh.contains(sid)).collect();
        let sids: Vec<String> = candidates.iter().map(|(sid, _)| sid.clone()).collect();
        let rows = self.run(&format!("{} UNWIND $sids AS s MATCH (n:Code {{sid: s}}) WHERE n:Sym RETURN {}", layer::OBSERVED_READ, sym_cols("n")), params! { "sids" => sids }).await?;
        let by_sid: HashMap<String, SymbolRef> = rows.rows.iter().map(|r| (s(r, 0), sym_from(r, 0))).collect();
        let describable = ["function", "method", "class", "struct", "trait", "interface", "enum", "type", "component", "macro", "module", "route", "command", "shell_function"];
        let mut hits = Vec::new();
        for (sid, score) in candidates {
            if hits.len() >= limit as usize {
                break;
            }
            if let Some(symbol) = by_sid.get(&sid)
                && describable.contains(&symbol.kind.as_str())
            {
                hits.push(SearchHit { symbol: symbol.clone(), score, matched: "centrality".into() });
            }
        }
        Ok(SearchResult { hits })
    }

    /// Whether IronGraph has embedded the most recently written symbol (search finds it by its
    /// own name and signature). IronGraph embeds asynchronously in write order.
    pub async fn embedding_ready(&self) -> Result<bool, RpcError> {
        let probes = self.last_written.lock().await.clone();
        let mut done: Vec<String> = Vec::new();
        for (sid, text) in &probes {
            let rows = self
                .run(
                    &format!("{} MATCH (n:Sym {{sid: $sid}}) WITH n SEARCH n IN (EMBEDDING INDEX semantic_nodes FOR TEXT $text LIMIT 5) SCORE AS score RETURN n.sid", layer::OBSERVED_READ),
                    params! { "sid" => sid, "text" => text },
                )
                .await?;
            // A symbol deleted since it was written no longer needs embedding.
            let exists = self.run(&format!("{} MATCH (n:Sym {{sid: $sid}}) RETURN n.sid", layer::OBSERVED_READ), params! { "sid" => sid }).await?;
            if rows.column(0).any(|v| v.as_str() == Some(sid.as_str())) || exists.rows.is_empty() {
                done.push(sid.clone());
            }
        }
        let mut pending = self.last_written.lock().await;
        pending.retain(|(sid, _)| !done.contains(sid));
        Ok(pending.is_empty())
    }
}

/// Tokens of repository-map text as the model counts them: `model.count` of a system message
/// holding the text, less that of an empty one (the model's own chat template and tokenizer, as the
/// chat flow counts its prompts). Without a model that answers, the text's bytes: every token of a
/// byte-level tokenizer covers at least one byte, so bytes never undercount.
pub struct MapCounter {
    host: Option<Arc<dyn Host>>,
    /// Tokens of the empty message; None: the model did not answer.
    empty: Option<u32>,
}

impl MapCounter {
    pub async fn new(host: Option<Arc<dyn Host>>) -> Self {
        let empty = match &host {
            Some(host) => Self::count(host.as_ref(), "").await,
            None => None,
        };
        Self { host, empty }
    }

    async fn count(host: &dyn Host, text: &str) -> Option<u32> {
        let params = serde_json::json!({"messages": [{"role": "system", "content": text}]});
        let value = host.call(cc_protocol::contracts::model::methods::COUNT, params).await.ok()?;
        serde_json::from_value::<cc_protocol::contracts::model::CountResult>(value).ok().map(|counted| counted.tokens)
    }

    /// Whether a model counts (false: bytes).
    pub fn by_model(&self) -> bool {
        self.empty.is_some()
    }

    pub async fn tokens(&self, text: &str) -> u32 {
        if let (Some(host), Some(empty)) = (&self.host, self.empty)
            && let Some(tokens) = Self::count(host.as_ref(), text).await
        {
            return tokens.saturating_sub(empty);
        }
        text.len() as u32
    }
}

fn first_line<'a>(signature: &'a str, name: &'a str) -> &'a str {
    let line = signature.lines().next().unwrap_or("").trim();
    if line.is_empty() { name } else { line }
}
