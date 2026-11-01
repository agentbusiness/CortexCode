//! Cited notes in the KNOWLEDGE layer: `(:Note {id, name, topic, topic_key, claim, quote, url,
//! title, date, source, question, by, project_id, created_ms})-[:CITES]->(:Source {url, name,
//! title, date})`, each note `-[:IN]->` the scope's root (see `work`). Global notes live in the
//! main project, a project's notes in its own IronGraph project. Search by meaning is IronGraph's
//! automatic embedding index (`semantic_nodes`) bound to the scope's live `Note` nodes (of one
//! topic when asked) before it ranks them.
//!
//! Nothing is deleted: a removed note is retired (`ended_ms`, `ended_reason`, `state: 'gone'`),
//! and so is a source no live note cites any more; adding the note again brings both back. Every
//! node and edge keeps `first_ms` and `last_ms`.

use crate::db::{Rows, layer};
use crate::params;
use crate::work::{WorkStore, revive, span};
use cc_protocol::RpcError;
use cc_protocol::contracts::memory::{Note, NoteAddParams, NoteSearchResult};
use cc_protocol::contracts::now_ms;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// A note's creation time: the clock, but always later than the previous note's, so notes added in
/// the same millisecond still list in the order they were added.
fn created_ms() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = now_ms();
    let previous = LAST.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| Some(now.max(last + 1))).unwrap_or(0);
    now.max(previous + 1)
}

const NOTE_COLS: &str = "n.id, n.project_id, n.topic, n.claim, n.quote, n.url, n.title, n.date, n.source, n.question, n.by, n.created_ms";

fn s(row: &[Value], i: usize) -> String {
    row.get(i).and_then(Value::as_str).unwrap_or("").to_owned()
}

fn opt_s(row: &[Value], i: usize) -> Option<String> {
    row.get(i).and_then(Value::as_str).map(str::to_owned)
}

fn n(row: &[Value], i: usize) -> u64 {
    row.get(i).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64))).unwrap_or(0)
}

/// Whitespace runs as one space, trimmed.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What topics are compared by: lowercase letters and digits, every other run one `-`
/// (`LLM inference`, `llm-inference` and `LLM  Inference!` are one topic).
pub fn topic_key(topic: &str) -> String {
    let mut key = String::new();
    for c in topic.chars() {
        if c.is_alphanumeric() {
            key.extend(c.to_lowercase());
        } else if !key.is_empty() && !key.ends_with('-') {
            key.push('-');
        }
    }
    key.trim_end_matches('-').to_owned()
}

/// `n-` + 16 hex of SHA-256 of scope (empty for global), topic key, url and quote.
pub fn note_id(scope: &str, topic: &str, url: &str, quote: &str) -> String {
    let digest = Sha256::digest(format!("{scope}\n{}\n{}\n{}", topic_key(topic), url.trim(), squash(quote)).as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("n-{hex}")
}

fn note_from(row: &[Value]) -> Note {
    Note {
        id: s(row, 0),
        project_id: opt_s(row, 1).filter(|p| !p.is_empty()),
        topic: s(row, 2),
        claim: s(row, 3),
        quote: s(row, 4),
        url: s(row, 5),
        title: s(row, 6),
        date: opt_s(row, 7),
        source: opt_s(row, 8),
        question: opt_s(row, 9),
        by: opt_s(row, 10),
        created_ms: n(row, 11),
        score: None,
    }
}

/// `MATCH` of the scope's notes, of one topic when given, with its parameters.
fn bind(topic: Option<&str>) -> (String, Map<String, Value>) {
    match topic.map(topic_key).filter(|key| !key.is_empty()) {
        Some(key) => ("MATCH (n:Note {topic_key: $topic}) WHERE n.ended_ms IS NULL".to_owned(), params! { "topic" => key }),
        None => ("MATCH (n:Note) WHERE n.ended_ms IS NULL".to_owned(), Map::new()),
    }
}

impl WorkStore {
    async fn note_run(&self, cypher: &str, params: Map<String, Value>) -> Result<Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// The scope's project id for a note (None for the global scope).
    fn note_project(&self) -> Option<String> {
        (self.scope != "global").then(|| self.scope.clone())
    }

    pub async fn note_get(&self, id: &str) -> Result<Option<Note>, RpcError> {
        let rows = self.note_run(&format!("{} MATCH (n:Note {{id: $id}}) WHERE n.ended_ms IS NULL RETURN {NOTE_COLS}", layer::KNOWLEDGE_READ), params! { "id" => id }).await?;
        Ok(rows.rows.first().map(|row| note_from(row)))
    }

    pub async fn note_add(&self, p: NoteAddParams) -> Result<Note, RpcError> {
        let required = [("topic", topic_key(&p.topic)), ("claim", p.claim.trim().to_owned()), ("quote", p.quote.trim().to_owned()), ("url", p.url.trim().to_owned())];
        if let Some((name, _)) = required.iter().find(|(_, value)| value.is_empty()) {
            return Err(RpcError::invalid_params(format!("a note needs `{name}`")));
        }
        let project = self.note_project();
        let topic = squash(&p.topic);
        let (url, quote, title) = (p.url.trim().to_owned(), p.quote.trim().to_owned(), squash(&p.title));
        let id = note_id(project.as_deref().unwrap_or(""), &topic, &url, &quote);
        if let Some(existing) = self.note_get(&id).await? {
            return Ok(existing);
        }
        let claim = squash(&p.claim);
        let date = p.date.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty());
        // A KNOWLEDGE node needs its `name` when it is created, and MERGE keys on it: the name
        // comes from what the id is made of (the quote's start; a source's url), so two writers of
        // one note (or one source) leave one node. No live note has this id here, so the node is
        // new or a retired one written again (brought back with this content).
        let name: String = squash(&quote).chars().take(80).collect();
        self.note_run(
            &format!(
                "{} MERGE (n:Note {{id: $id, name: $name}}) SET n.project_id = $project, n.topic = $topic, n.topic_key = $key, n.claim = $claim, n.quote = $quote, n.url = $url, n.title = $title, n.date = $date, n.source = $source, n.question = $question, n.by = $by, n.created_ms = $now, {}, n.state = null, {} \
                 MERGE (c:Source {{url: $url, name: $url}}) ON CREATE SET c.title = $title, c.date = $date SET {}, c.state = null, {} \
                 MERGE (n)-[e:CITES]->(c) SET {}",
                layer::KNOWLEDGE_WRITE,
                revive("n"),
                span("n"),
                revive("c"),
                span("c"),
                span("e")
            ),
            params! {
                "id" => id, "name" => name, "project" => project.clone().unwrap_or_default(), "topic" => topic, "key" => topic_key(&topic),
                "claim" => claim, "quote" => quote, "url" => url, "title" => title, "date" => date, "source" => p.source, "question" => p.question,
                "by" => p.by, "now" => created_ms()
            },
        )
        .await?;
        self.link_root(layer::KNOWLEDGE_WRITE, "MATCH (w:Note {id: $id})", params! { "id" => id }).await?;
        for (name, label, property) in [("note_id", "Note", "id"), ("note_topic", "Note", "topic_key"), ("source_url", "Source", "url")] {
            self.graph.ensure_index(&self.ig_id, name, label, property).await?;
        }
        self.note_get(&id).await?.ok_or_else(|| RpcError::internal(format!("note {id} missing after it was written")))
    }

    /// A topic's notes (every topic's when None), oldest first.
    pub async fn note_list(&self, topic: Option<&str>) -> Result<Vec<Note>, RpcError> {
        let (matched, params) = bind(topic);
        let rows = self.note_run(&format!("{} {matched} RETURN {NOTE_COLS}", layer::KNOWLEDGE_READ), params).await?;
        let mut notes: Vec<Note> = rows.rows.iter().map(|row| note_from(row)).collect();
        notes.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
        Ok(notes)
    }

    /// Best matches by meaning first. Every note in scope is ranked (the scope is bound before
    /// IronGraph's SEARCH), so `pending` counts exactly the notes not embedded yet.
    pub async fn note_search(&self, text: &str, topic: Option<&str>, limit: u32) -> Result<NoteSearchResult, RpcError> {
        if text.trim().is_empty() {
            return Err(RpcError::invalid_params("memory.note.search needs `text`"));
        }
        let (matched, params) = bind(topic);
        let counted = self.note_run(&format!("{} {matched} RETURN count(n)", layer::KNOWLEDGE_READ), params.clone()).await?;
        let total = counted.rows.first().map(|row| n(row, 0)).unwrap_or(0);
        if total == 0 {
            return Ok(NoteSearchResult::default());
        }
        let mut search = params;
        search.insert("text".into(), Value::from(text.trim()));
        search.insert("k".into(), Value::from(total));
        let rows = self
            .note_run(&format!("{} {matched} WITH n SEARCH n IN (EMBEDDING INDEX semantic_nodes FOR TEXT $text LIMIT $k) SCORE AS score RETURN n.id, score", layer::KNOWLEDGE_READ), search)
            .await?;
        let mut seen = HashSet::new();
        let mut ranked: Vec<(String, f64)> =
            rows.rows.iter().map(|row| (s(row, 0), row.get(1).and_then(Value::as_f64).unwrap_or(0.0))).filter(|(id, _)| seen.insert(id.clone())).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let pending = total.saturating_sub(ranked.len() as u64);
        ranked.truncate(limit.max(1) as usize);
        let ids: Vec<&str> = ranked.iter().map(|(id, _)| id.as_str()).collect();
        let rows = self.note_run(&format!("{} UNWIND $ids AS i MATCH (n:Note {{id: i}}) WHERE n.ended_ms IS NULL RETURN {NOTE_COLS}", layer::KNOWLEDGE_READ), params! { "ids" => ids }).await?;
        let mut by_id: HashMap<String, Note> = rows.rows.iter().map(|row| note_from(row)).map(|note| (note.id.clone(), note)).collect();
        let notes = ranked
            .into_iter()
            .filter_map(|(id, score)| {
                by_id.remove(&id).map(|mut note| {
                    note.score = Some(score);
                    note
                })
            })
            .collect();
        Ok(NoteSearchResult { notes, pending })
    }

    /// Retire a note, and its source when no other live note cites it (their nodes and edges
    /// stay as history). False when there was none.
    pub async fn note_remove(&self, id: &str) -> Result<bool, RpcError> {
        let Some(note) = self.note_get(id).await? else { return Ok(false) };
        let now = now_ms();
        self.note_run(
            &format!("{} MATCH (n:Note {{id: $id}}) WHERE n.ended_ms IS NULL SET n.ended_ms = $now, n.ended_reason = 'removed', n.state = 'gone', n.last_ms = $now", layer::KNOWLEDGE_WRITE),
            params! { "id" => id, "now" => now },
        )
        .await?;
        self.note_run(
            &format!(
                "{} MATCH (c:Source {{url: $url}}) WHERE c.ended_ms IS NULL OPTIONAL MATCH (c)<-[:CITES]-(m:Note) WHERE m.ended_ms IS NULL WITH c, count(m) AS live WHERE live = 0 SET c.ended_ms = $now, c.ended_reason = 'no note cites it', c.state = 'gone', c.last_ms = $now",
                layer::KNOWLEDGE_WRITE
            ),
            params! { "url" => note.url, "now" => now },
        )
        .await?;
        Ok(true)
    }
}
