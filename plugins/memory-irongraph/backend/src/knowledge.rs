//! Project knowledge in the KNOWLEDGE layer (docs/PROJECT-KNOWLEDGE-PLAN.md P0): what the harness
//! knows about a project's world. One `(:Knowledge:<Kind> {id, name, text, state, source, ...})`
//! node per item, kinds `Concept | Fact | Decision | Hypothesis | Question`, in the project's own
//! IronGraph project (the global scope's in `main`). Links are edges whose `rel` property names the
//! relation: `IMPLEMENTED_BY` / `ABOUT` (-> `Code`), `DESCRIBED_IN` (-> a document `Section`),
//! `SUPPORTED_BY` (-> `Note`), `TESTED_BY` (-> `Run`, a WORKSPACE node), `ANSWERS` and
//! `SUPERSEDES` (-> `Knowledge`). Code is linked by sid, so a rename keeps the link.
//!
//! Nothing is deleted: a withdrawn or superseded item keeps its node and links as history, and
//! search, `linked` and the brief serve only live items. Search by meaning is IronGraph's automatic
//! embedding index (`semantic_nodes`) bound to the scope's live items, an exact name or alias match
//! first.

use crate::db::{Rows, layer};
use crate::host::Host;
use crate::notes::topic_key;
use crate::params;
use crate::queries::MapCounter;
use crate::work::WorkStore;
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{
    KnowledgeBriefResult, KnowledgeItem, KnowledgeLink, KnowledgeLinked, KnowledgeLinkedResult, KnowledgeSearchResult, KnowledgeUpsertParams, knowledge as k,
};
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

const COLS: &str = "k.id, k.project_id, k.kind, k.name, k.text, k.state, k.source, k.confidence, k.as_of, k.by, k.evidence, k.aliases, k.superseded_by, k.reason, k.created_ms, k.updated_ms";

fn s(row: &[Value], i: usize) -> String {
    row.get(i).and_then(Value::as_str).unwrap_or("").to_owned()
}

fn opt_s(row: &[Value], i: usize) -> Option<String> {
    row.get(i).and_then(Value::as_str).map(str::to_owned).filter(|v| !v.is_empty())
}

fn n(row: &[Value], i: usize) -> u64 {
    row.get(i).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0)
}

fn f(row: &[Value], i: usize) -> f32 {
    row.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32
}

fn list(row: &[Value], i: usize) -> Vec<String> {
    row.get(i).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

/// Whitespace runs as one space, trimmed.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `k-` + 16 hex of SHA-256 of kind, scope (empty for global) and the normalized name.
pub fn knowledge_id(kind: &str, scope: &str, name: &str) -> String {
    let digest = Sha256::digest(format!("{kind}\n{scope}\n{}", topic_key(name)).as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("k-{hex}")
}

/// `YYYY-MM-DD` (UTC) of a time in epoch milliseconds.
pub fn date_of_ms(ms: u64) -> String {
    // Howard Hinnant's civil-from-days.
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The one line the host appends to code an item is bound to:
/// `<kind>[ <state>] (<source>[, run <id>], <date>): <text>  knowledge://<id>`.
pub fn line(item: &KnowledgeItem) -> String {
    let state = match item.kind.as_str() {
        k::HYPOTHESIS | k::QUESTION => format!(" {}", item.state),
        _ if !k::is_live(&item.state) => format!(" {}", item.state),
        _ => String::new(),
    };
    let mut who = item.source.clone();
    if let Some(run) = item.evidence.iter().find_map(|e| e.strip_prefix("run://")) {
        who.push_str(&format!(", run {run}"));
    }
    let date = item.as_of.clone().unwrap_or_else(|| date_of_ms(item.created_ms));
    format!("{}{state} ({who}, {date}): {}  knowledge://{}", item.kind, item.text, item.id)
}

fn item_from(row: &[Value]) -> KnowledgeItem {
    KnowledgeItem {
        id: s(row, 0),
        project_id: opt_s(row, 1),
        kind: s(row, 2),
        name: s(row, 3),
        text: s(row, 4),
        state: s(row, 5),
        source: s(row, 6),
        confidence: f(row, 7),
        as_of: opt_s(row, 8),
        by: s(row, 9),
        evidence: list(row, 10),
        aliases: list(row, 11),
        superseded_by: opt_s(row, 12),
        reason: opt_s(row, 13),
        created_ms: n(row, 14),
        updated_ms: n(row, 15),
        links: Vec::new(),
        score: None,
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        k::CONCEPT => "Concept",
        k::FACT => "Fact",
        k::DECISION => "Decision",
        k::HYPOTHESIS => "Hypothesis",
        k::QUESTION => "Question",
        _ => "Knowledge",
    }
}

fn edge_type(rel: &str) -> String {
    rel.to_ascii_uppercase()
}

fn check_kind(kind: &str) -> Result<(), RpcError> {
    if k::KINDS.contains(&kind) { Ok(()) } else { Err(RpcError::invalid_params(format!("unknown knowledge kind `{kind}`; one of {}", k::KINDS.join(", ")))) }
}

fn check_state(kind: &str, state: &str) -> Result<(), RpcError> {
    if k::states(kind).contains(&state) {
        Ok(())
    } else {
        Err(RpcError::invalid_params(format!("a {kind} cannot be `{state}`; one of {}", k::states(kind).join(", "))))
    }
}

fn check_rel(rel: &str) -> Result<(), RpcError> {
    if k::RELATIONS.contains(&rel) || rel == "supersedes" {
        Ok(())
    } else {
        Err(RpcError::invalid_params(format!("unknown knowledge relation `{rel}`; one of {}", k::RELATIONS.join(", "))))
    }
}

fn union(into: &mut Vec<String>, more: &[String]) {
    for item in more {
        let item = item.trim();
        if !item.is_empty() && !into.iter().any(|x| x == item) {
            into.push(item.to_owned());
        }
    }
}

/// Aliases compare by key (`hdcp` and `HDCP` are one alias).
fn union_aliases(into: &mut Vec<String>, more: &[String], name: &str) {
    for alias in more {
        let alias = squash(alias);
        let key = topic_key(&alias);
        if !key.is_empty() && key != topic_key(name) && !into.iter().any(|x| topic_key(x) == key) {
            into.push(alias);
        }
    }
}

/// A resolved link target: the clause that matches it (binding `t` by `$target` = `key`) and the
/// layers to read it in and write the edge to.
struct Target {
    matched: &'static str,
    layers: &'static str,
    key: String,
}

impl WorkStore {
    async fn k_run(&self, cypher: &str, params: Map<String, Value>) -> Result<Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// The scope's project id (None for the global scope).
    fn k_project(&self) -> Option<String> {
        (self.scope != "global").then(|| self.scope.clone())
    }

    pub async fn knowledge_get(&self, id: &str) -> Result<Option<KnowledgeItem>, RpcError> {
        let rows = self.k_run(&format!("{} MATCH (k:Knowledge {{id: $id}}) RETURN {COLS}", layer::KNOWLEDGE_READ), params! { "id" => id }).await?;
        let Some(item) = rows.rows.first().map(|row| item_from(row)) else { return Ok(None) };
        Ok(self.with_links(vec![item]).await?.pop())
    }

    async fn must_get(&self, id: &str) -> Result<KnowledgeItem, RpcError> {
        self.knowledge_get(id).await?.ok_or_else(|| RpcError::not_found(format!("knowledge item {id} not found")))
    }

    /// The items with their links (one query for all of them).
    async fn with_links(&self, mut items: Vec<KnowledgeItem>) -> Result<Vec<KnowledgeItem>, RpcError> {
        if items.is_empty() {
            return Ok(items);
        }
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        let rows = self
            .k_run(
                &format!("{} UNWIND $ids AS i MATCH (k:Knowledge {{id: i}})-[r]->(t) RETURN i, r.rel, r.ref, t.sid, t.id, t.name, t.qname", layer::ALL_READ),
                params! { "ids" => ids },
            )
            .await?;
        let mut by_id: HashMap<String, Vec<KnowledgeLink>> = HashMap::new();
        for row in &rows.rows {
            let rel = s(row, 1);
            if rel.is_empty() {
                continue;
            }
            let id = opt_s(row, 3).or_else(|| opt_s(row, 4)).unwrap_or_default();
            let name = opt_s(row, 6).or_else(|| opt_s(row, 5)).unwrap_or_else(|| id.clone());
            by_id.entry(s(row, 0)).or_default().push(KnowledgeLink { rel, target: s(row, 2), id, name });
        }
        for item in &mut items {
            if let Some(mut links) = by_id.remove(&item.id) {
                links.sort_by(|a, b| (&a.rel, &a.id).cmp(&(&b.rel, &b.id)));
                item.links = links;
            }
        }
        Ok(items)
    }

    /// Where a link's target lives, if it exists.
    async fn resolve_target(&self, rel: &str, target: &str) -> Result<Option<Target>, RpcError> {
        let target = target.trim();
        if target.is_empty() {
            return Ok(None);
        }
        let found = match rel {
            "implemented_by" | "about" => match self.code_sid(target).await? {
                Some(sid) => {
                    let rows = self.k_run(&format!("{} MATCH (t:Code {{sid: $v}}) RETURN t.sid, t.qname, t.name", layer::OBSERVED_READ), params! { "v" => sid }).await?;
                    rows.rows.first().map(|row| Target {
                        matched: "MATCH (t:Code {sid: $target})",
                        layers: layer::KNOWLEDGE_WRITE,
                        key: s(row, 0),
                    })
                }
                None => None,
            },
            "described_in" => {
                let rows = self.k_run(&format!("{} MATCH (t:Section {{sid: $v}}) RETURN t.sid, t.name", layer::OBSERVED_READ), params! { "v" => target }).await?;
                rows.rows.first().map(|row| Target { matched: "MATCH (t:Section {sid: $target})", layers: layer::KNOWLEDGE_WRITE, key: s(row, 0) })
            }
            "supported_by" => {
                let id = target.strip_prefix("note://").unwrap_or(target);
                let rows = self.k_run(&format!("{} MATCH (t:Note {{id: $v}}) RETURN t.id, t.claim", layer::KNOWLEDGE_READ), params! { "v" => id }).await?;
                rows.rows.first().map(|row| Target { matched: "MATCH (t:Note {id: $target})", layers: layer::KNOWLEDGE_WRITE, key: s(row, 0) })
            }
            "tested_by" => {
                let id = target.strip_prefix("run://").unwrap_or(target);
                let rows = self.k_run(&format!("{} MATCH (t:Run {{id: $v}}) RETURN t.id, t.outcome", layer::WORKSPACE_READ), params! { "v" => id }).await?;
                rows.rows.first().map(|row| Target { matched: "MATCH (t:Run {id: $target})", layers: layer::WORKSPACE_WRITE, key: s(row, 0) })
            }
            "answers" | "supersedes" => {
                let id = target.strip_prefix("knowledge://").unwrap_or(target);
                let rows = self.k_run(&format!("{} MATCH (t:Knowledge {{id: $v}}) RETURN t.id, t.name", layer::KNOWLEDGE_READ), params! { "v" => id }).await?;
                rows.rows.first().map(|row| Target { matched: "MATCH (t:Knowledge {id: $target})", layers: layer::KNOWLEDGE_WRITE, key: s(row, 0) })
            }
            _ => None,
        };
        Ok(found)
    }

    /// Add one link; false when the target does not resolve.
    async fn add_link(&self, id: &str, rel: &str, target: &str) -> Result<bool, RpcError> {
        check_rel(rel)?;
        let Some(found) = self.resolve_target(rel, target).await? else { return Ok(false) };
        self.k_run(
            &format!("{} MATCH (k:Knowledge {{id: $id}}) {} MERGE (k)-[r:{} {{rel: $rel}}]->(t) SET r.ref = $ref", found.layers, found.matched, edge_type(rel)),
            params! { "id" => id, "target" => found.key, "rel" => rel, "ref" => target.trim() },
        )
        .await?;
        Ok(true)
    }

    pub async fn knowledge_upsert(&self, p: KnowledgeUpsertParams) -> Result<KnowledgeItem, RpcError> {
        let kind = p.kind.trim().to_ascii_lowercase();
        check_kind(&kind)?;
        let name = squash(&p.name);
        let text = squash(&p.text);
        if name.is_empty() {
            return Err(RpcError::invalid_params("a knowledge item needs `name`"));
        }
        let source = p.source.as_deref().map(str::trim).filter(|v| !v.is_empty()).unwrap_or("user").to_owned();
        if !k::SOURCES.contains(&source.as_str()) {
            return Err(RpcError::invalid_params(format!("unknown knowledge source `{source}`; one of {}", k::SOURCES.join(", "))));
        }
        let state = p.state.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        if let Some(state) = &state {
            check_state(&kind, state)?;
        }
        let project = self.k_project();
        let id = knowledge_id(&kind, project.as_deref().unwrap_or(""), &name);
        let now = now_ms();
        let as_of = p.as_of.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty());
        let mut aliases: Vec<String> = Vec::new();
        union_aliases(&mut aliases, &p.aliases, &name);
        let alias_keys: Vec<String> = aliases.iter().map(|a| topic_key(a)).collect();
        let mut evidence: Vec<String> = Vec::new();
        union(&mut evidence, &p.evidence);
        if let Some(existing) = self.knowledge_get(&id).await? {
            if text.is_empty() && state.is_none() && p.confidence.is_none() && as_of.is_none() && evidence.is_empty() && aliases.is_empty() && p.links.is_empty() {
                return Ok(existing);
            }
            let mut merged_evidence = existing.evidence.clone();
            union(&mut merged_evidence, &evidence);
            let mut merged_aliases = existing.aliases.clone();
            union_aliases(&mut merged_aliases, &aliases, &name);
            let merged_keys: Vec<String> = merged_aliases.iter().map(|a| topic_key(a)).collect();
            self.k_run(
                &format!(
                    "{} MATCH (k:Knowledge {{id: $id}}) SET k.text = $text, k.state = $state, k.source = $source, k.confidence = $confidence, k.as_of = $as_of, k.by = $by, k.evidence = $evidence, k.aliases = $aliases, k.alias_keys = $alias_keys, k.updated_ms = $now",
                    layer::KNOWLEDGE_WRITE
                ),
                params! {
                    "id" => id, "text" => if text.is_empty() { existing.text.clone() } else { text.clone() },
                    "state" => state.clone().unwrap_or_else(|| existing.state.clone()),
                    "source" => if p.source.is_some() { source.clone() } else { existing.source.clone() },
                    "confidence" => p.confidence.unwrap_or(existing.confidence),
                    "as_of" => as_of.clone().or_else(|| existing.as_of.clone()),
                    "by" => if p.by.trim().is_empty() { existing.by.clone() } else { p.by.trim().to_owned() },
                    "evidence" => merged_evidence, "aliases" => merged_aliases, "alias_keys" => merged_keys, "now" => now
                },
            )
            .await?;
        } else {
            if text.is_empty() {
                return Err(RpcError::invalid_params("a knowledge item needs `text`"));
            }
            let state = state.unwrap_or_else(|| k::default_state(&kind).to_owned());
            let confidence = p.confidence.unwrap_or(if source == "user" { 1.0 } else { 0.5 });
            // A KNOWLEDGE node needs its `name` when it is created, and MERGE keys on it, so two
            // writers of one item leave one node.
            self.k_run(
                &format!(
                    "{} MERGE (k:Knowledge:{} {{id: $id, name: $name}}) ON CREATE SET k.project_id = $project, k.kind = $kind, k.name_key = $name_key, k.text = $text, k.state = $state, k.source = $source, k.confidence = $confidence, k.as_of = $as_of, k.by = $by, k.evidence = $evidence, k.aliases = $aliases, k.alias_keys = $alias_keys, k.created_ms = $now, k.updated_ms = $now",
                    layer::KNOWLEDGE_WRITE,
                    kind_label(&kind)
                ),
                params! {
                    "id" => id, "name" => name, "project" => project.clone().unwrap_or_default(), "kind" => kind, "name_key" => topic_key(&name), "text" => text,
                    "state" => state, "source" => source, "confidence" => confidence, "as_of" => as_of, "by" => p.by.trim(), "evidence" => evidence,
                    "aliases" => aliases, "alias_keys" => alias_keys, "now" => now
                },
            )
            .await?;
            for (index, label, property) in [("knowledge_id", "Knowledge", "id"), ("knowledge_kind", "Knowledge", "kind"), ("knowledge_state", "Knowledge", "state")] {
                self.graph.ensure_index(&self.ig_id, index, label, property).await?;
            }
        }
        let mut unresolved: Vec<KnowledgeLink> = Vec::new();
        for link in &p.links {
            if !self.add_link(&id, link.rel.trim(), &link.target).await? {
                unresolved.push(KnowledgeLink { rel: link.rel.trim().to_owned(), target: link.target.trim().to_owned(), id: String::new(), name: String::new() });
            }
        }
        let mut item = self.must_get(&id).await?;
        item.links.extend(unresolved);
        Ok(item)
    }

    /// Items of the scope, newest first; live ones unless `include_history`.
    pub async fn knowledge_list(&self, kind: Option<&str>, state: Option<&str>, include_history: bool) -> Result<Vec<KnowledgeItem>, RpcError> {
        let rows = self.k_run(&format!("{} MATCH (k:Knowledge) RETURN {COLS}", layer::KNOWLEDGE_READ), Map::new()).await?;
        let kind = kind.map(str::trim).filter(|v| !v.is_empty()).map(str::to_ascii_lowercase);
        let state = state.map(str::trim).filter(|v| !v.is_empty()).map(str::to_ascii_lowercase);
        let mut items: Vec<KnowledgeItem> = rows
            .rows
            .iter()
            .map(|row| item_from(row))
            .filter(|item| kind.as_ref().is_none_or(|kind| &item.kind == kind))
            .filter(|item| state.as_ref().is_none_or(|state| &item.state == state))
            .filter(|item| include_history || state.is_some() || k::is_live(&item.state))
            .collect();
        items.sort_by(|a, b| (b.updated_ms, &b.id).cmp(&(a.updated_ms, &a.id)));
        self.with_links(items).await
    }

    /// Live items by meaning, best first; an exact name or alias match first with score 1.
    /// Every live item in scope is ranked, so `pending` counts exactly the ones not embedded yet.
    pub async fn knowledge_search(&self, text: &str, kind: Option<&str>, limit: u32) -> Result<KnowledgeSearchResult, RpcError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(RpcError::invalid_params("memory.knowledge.search needs `text`"));
        }
        let live = self.knowledge_list(kind, None, false).await?;
        if live.is_empty() {
            return Ok(KnowledgeSearchResult::default());
        }
        let key = topic_key(text);
        let mut items: Vec<KnowledgeItem> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for item in &live {
            if !key.is_empty() && (topic_key(&item.name) == key || item.aliases.iter().any(|a| topic_key(a) == key)) && seen.insert(item.id.clone()) {
                let mut hit = item.clone();
                hit.score = Some(1.0);
                items.push(hit);
            }
        }
        let ids: Vec<&str> = live.iter().map(|i| i.id.as_str()).collect();
        let rows = self
            .k_run(
                &format!("{} UNWIND $ids AS i MATCH (k:Knowledge {{id: i}}) WITH k SEARCH k IN (EMBEDDING INDEX semantic_nodes FOR TEXT $text LIMIT $k) SCORE AS score RETURN k.id, score", layer::KNOWLEDGE_READ),
                params! { "ids" => ids, "text" => text, "k" => live.len() },
            )
            .await?;
        let mut ranked: Vec<(String, f64)> = Vec::new();
        let mut ranked_ids: HashSet<String> = HashSet::new();
        for row in &rows.rows {
            let id = s(row, 0);
            if ranked_ids.insert(id.clone()) {
                ranked.push((id, row.get(1).and_then(Value::as_f64).unwrap_or(0.0)));
            }
        }
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let pending = (live.len() as u64).saturating_sub(ranked.len() as u64);
        let by_id: HashMap<&str, &KnowledgeItem> = live.iter().map(|i| (i.id.as_str(), i)).collect();
        for (id, score) in ranked {
            if items.len() >= limit.max(1) as usize {
                break;
            }
            if let Some(item) = by_id.get(id.as_str())
                && seen.insert(id.clone())
            {
                let mut hit = (*item).clone();
                hit.score = Some(score);
                items.push(hit);
            }
        }
        items.truncate(limit.max(1) as usize);
        Ok(KnowledgeSearchResult { items, pending })
    }

    pub async fn knowledge_link(&self, id: &str, rel: &str, target: &str) -> Result<KnowledgeItem, RpcError> {
        let rel = rel.trim().to_ascii_lowercase();
        self.must_get(id).await?;
        if !self.add_link(id, &rel, target).await? {
            return Err(RpcError::not_found(format!("`{target}` does not name a {} target", rel.replace('_', " "))));
        }
        self.must_get(id).await
    }

    pub async fn knowledge_unlink(&self, id: &str, rel: &str, target: &str) -> Result<KnowledgeItem, RpcError> {
        let rel = rel.trim().to_ascii_lowercase();
        check_rel(&rel)?;
        self.must_get(id).await?;
        let target = target.trim();
        // Edges to WORKSPACE nodes live in that layer; the rest in KNOWLEDGE.
        for layers in [layer::KNOWLEDGE_WRITE, layer::WORKSPACE_WRITE] {
            self.k_run(
                &format!("{} MATCH (k:Knowledge {{id: $id}})-[r {{rel: $rel}}]->(t) WHERE r.ref = $ref OR t.sid = $ref OR t.id = $ref DELETE r", layers),
                params! { "id" => id, "rel" => rel, "ref" => target },
            )
            .await?;
        }
        self.must_get(id).await
    }

    pub async fn knowledge_withdraw(&self, id: &str, reason: &str, by: &str) -> Result<KnowledgeItem, RpcError> {
        let existing = self.must_get(id).await?;
        let by = if by.trim().is_empty() { existing.by } else { by.trim().to_owned() };
        self.k_run(
            &format!("{} MATCH (k:Knowledge {{id: $id}}) SET k.state = $state, k.reason = $reason, k.by = $by, k.updated_ms = $now", layer::KNOWLEDGE_WRITE),
            params! { "id" => id, "state" => k::WITHDRAWN, "reason" => squash(reason), "by" => by, "now" => now_ms() },
        )
        .await?;
        self.must_get(id).await
    }

    /// `by` supersedes `id`: the old item is `superseded`, names its successor, and the successor
    /// `SUPERSEDES` it. Returns the superseded item.
    pub async fn knowledge_supersede(&self, id: &str, by: &str) -> Result<KnowledgeItem, RpcError> {
        let by = by.trim().strip_prefix("knowledge://").unwrap_or(by.trim()).to_owned();
        if by == id {
            return Err(RpcError::invalid_params("an item cannot supersede itself"));
        }
        self.must_get(id).await?;
        self.must_get(&by).await?;
        self.k_run(
            &format!("{} MATCH (k:Knowledge {{id: $id}}) SET k.state = $state, k.superseded_by = $by, k.updated_ms = $now", layer::KNOWLEDGE_WRITE),
            params! { "id" => id, "state" => k::SUPERSEDED, "by" => by, "now" => now_ms() },
        )
        .await?;
        self.add_link(&by, "supersedes", id).await?;
        self.must_get(id).await
    }

    /// Live items bound to the given code: by sid, or to the file or any symbol of the given paths.
    pub async fn knowledge_linked(&self, sids: &[String], paths: &[String]) -> Result<KnowledgeLinkedResult, RpcError> {
        let sids: Vec<String> = sids.iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
        let paths: Vec<String> = paths.iter().map(|p| p.trim().trim_start_matches("./").to_owned()).filter(|p| !p.is_empty()).collect();
        if sids.is_empty() && paths.is_empty() {
            return Ok(KnowledgeLinkedResult::default());
        }
        // (code sid, rel, item id), in the order the code was asked for.
        let mut bound: Vec<(String, String, String)> = Vec::new();
        if !sids.is_empty() {
            let rows = self
                .k_run(&format!("{} UNWIND $sids AS s MATCH (k:Knowledge)-[r]->(t {{sid: s}}) RETURN t.sid, r.rel, k.id", layer::CODE_READ), params! { "sids" => sids.clone() })
                .await?;
            bound.extend(rows.rows.iter().map(|row| (s(row, 0), s(row, 1), s(row, 2))));
        }
        if !paths.is_empty() {
            for (clause, values) in [("MATCH (k:Knowledge)-[r]->(t:Code {path: p})", paths.clone()), ("MATCH (k:Knowledge)-[r]->(t:Code {sid: p})", paths.iter().map(|p| format!("file:{p}")).collect())] {
                let rows = self.k_run(&format!("{} UNWIND $paths AS p {clause} RETURN t.sid, r.rel, k.id", layer::CODE_READ), params! { "paths" => values }).await?;
                bound.extend(rows.rows.iter().map(|row| (s(row, 0), s(row, 1), s(row, 2))));
            }
        }
        bound.retain(|(_, rel, _)| matches!(rel.as_str(), "implemented_by" | "about" | "described_in"));
        let ids: Vec<String> = bound.iter().map(|(_, _, id)| id.clone()).collect::<HashSet<_>>().into_iter().collect();
        if ids.is_empty() {
            return Ok(KnowledgeLinkedResult::default());
        }
        let rows = self.k_run(&format!("{} UNWIND $ids AS i MATCH (k:Knowledge {{id: i}}) RETURN {COLS}", layer::KNOWLEDGE_READ), params! { "ids" => ids }).await?;
        let items = self.with_links(rows.rows.iter().map(|row| item_from(row)).filter(|item| k::is_live(&item.state)).collect()).await?;
        let by_id: HashMap<&str, &KnowledgeItem> = items.iter().map(|i| (i.id.as_str(), i)).collect();
        let order: Vec<&String> = sids.iter().chain(paths.iter()).collect();
        let position = |sid: &str| {
            order.iter().position(|asked| **asked == sid || sid.strip_prefix("file:") == Some(asked.as_str())).unwrap_or(order.len())
        };
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut out: Vec<KnowledgeLinked> = bound
            .into_iter()
            .filter(|(sid, _, id)| seen.insert((sid.clone(), id.clone())))
            .filter_map(|(sid, rel, id)| by_id.get(id.as_str()).map(|item| KnowledgeLinked { sid, rel, line: line(item), item: (*item).clone() }))
            .collect();
        out.sort_by(|a, b| (position(&a.sid), &a.sid, &a.item.kind, &a.item.name).cmp(&(position(&b.sid), &b.sid, &b.item.kind, &b.item.name)));
        Ok(KnowledgeLinkedResult { items: out })
    }

    /// What every session of the project should know, within `budget_tokens`: accepted decisions,
    /// then facts (the user's first), concepts by how much code they bind, open questions; newest
    /// first within a group; one line each, no ids (the Knowledge section has them).
    pub async fn knowledge_brief(&self, budget_tokens: u32, host: Option<Arc<dyn Host>>) -> Result<KnowledgeBriefResult, RpcError> {
        if budget_tokens == 0 {
            return Ok(KnowledgeBriefResult::default());
        }
        let live = self.knowledge_list(None, None, false).await?;
        if live.is_empty() {
            return Ok(KnowledgeBriefResult::default());
        }
        let code_links = |item: &KnowledgeItem| item.links.iter().filter(|l| matches!(l.rel.as_str(), "implemented_by" | "about")).count();
        let mut groups: BTreeMap<u8, Vec<&KnowledgeItem>> = BTreeMap::new();
        for item in &live {
            let group = match item.kind.as_str() {
                k::DESCRIPTION => 0,
                k::DECISION => 1,
                k::FACT => 2,
                k::CONCEPT => 3,
                k::QUESTION if item.state == "open" => 4,
                k::HYPOTHESIS if item.state != "open" => 5,
                _ => continue,
            };
            groups.entry(group).or_default().push(item);
        }
        let mut lines: Vec<(String, String)> = Vec::new();
        for (group, mut items) in groups {
            match group {
                2 => items.sort_by(|a, b| ((a.source != "user") as u8, std::cmp::Reverse(a.updated_ms)).cmp(&((b.source != "user") as u8, std::cmp::Reverse(b.updated_ms)))),
                3 => items.sort_by(|a, b| (std::cmp::Reverse(code_links(a)), std::cmp::Reverse(a.updated_ms)).cmp(&(std::cmp::Reverse(code_links(b)), std::cmp::Reverse(b.updated_ms)))),
                _ => items.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms)),
            }
            for item in items {
                let text = match item.kind.as_str() {
                    // The project's description is the brief's opening paragraph, not a bullet.
                    k::DESCRIPTION => format!("{}\n", item.text.trim()),
                    k::CONCEPT => format!("- {}: {}", item.name, item.text),
                    k::HYPOTHESIS => format!("- {} ({}): {}", item.name, item.state, item.text),
                    k::QUESTION => format!("- open question: {}", item.text),
                    k::DECISION => format!("- decided: {}", item.text),
                    _ => format!("- {}", item.text),
                };
                lines.push((item.id.clone(), text));
            }
        }
        let counter = MapCounter::new(host).await;
        let render = |n: usize| lines[..n].iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>().join("\n");
        let mut fits = 0usize;
        let mut tokens = 0u32;
        let full = counter.tokens(&render(lines.len())).await;
        if full <= budget_tokens {
            fits = lines.len();
            tokens = full;
        } else {
            // The longest prefix of lines within the budget (tokens grow with lines).
            let (mut low, mut high) = (0usize, lines.len());
            while low < high {
                let mid = (low + high).div_ceil(2);
                let counted = counter.tokens(&render(mid)).await;
                if counted <= budget_tokens {
                    low = mid;
                    fits = mid;
                    tokens = counted;
                } else {
                    high = mid - 1;
                }
            }
        }
        Ok(KnowledgeBriefResult { text: render(fits), ids: lines[..fits].iter().map(|(id, _)| id.clone()).collect(), tokens, by_model: counter.by_model() })
    }
}
