//! Cited research notes, kept by the memory plugin in IronGraph's KNOWLEDGE layer
//! (`memory.note.*`): global unless a project id is given, searched by meaning with IronGraph's
//! own embedding. When memory cannot be reached, every note method fails with that reason; there
//! is no other store.
//!
//! Notes an earlier version kept as JSON lines (`<data>/notes/<project id>.jsonl`) are imported
//! once, as global notes, before the first note call: a file is renamed `<name>.jsonl.imported`
//! once all its notes are in memory, so it is never read again.

use crate::peers::Peers;
use crate::text::{normalize_date, squash};
use cc_protocol::contracts::memory::{self, methods};
use cc_protocol::contracts::research::{Note, NoteAddParams};
use cc_protocol::{RpcError, codes, parse_params, to_value};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

pub struct Notes {
    peers: Arc<dyn Peers>,
    /// Where an earlier version kept notes as JSON lines (imported once).
    legacy: PathBuf,
    imported: OnceCell<()>,
}

/// Notes found by a search, and the notes in scope memory has not embedded yet.
pub struct Found {
    pub notes: Vec<Note>,
    pub pending: u64,
}

fn scope(project_id: &str) -> Option<String> {
    Some(project_id.trim().to_owned()).filter(|id| !id.is_empty())
}

fn note_from(note: memory::Note) -> Note {
    Note {
        id: note.id,
        project_id: note.project_id.unwrap_or_default(),
        topic: note.topic,
        claim: note.claim,
        quote: note.quote,
        url: note.url,
        title: note.title,
        date: note.date,
        source: note.source,
        question: note.question,
        by: note.by,
        created_ms: note.created_ms,
        score: note.score.map(|s| (s * 1000.0).round() / 1000.0),
    }
}

impl Notes {
    pub fn new(peers: Arc<dyn Peers>, legacy: PathBuf) -> Self {
        Self { peers, legacy, imported: OnceCell::new() }
    }

    async fn memory<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: &P) -> Result<R, RpcError> {
        let value = self.peers.call(method, to_value(params)?, CancellationToken::new()).await.map_err(|error| {
            let code = if matches!(error.code.as_str(), codes::UNKNOWN_METHOD | codes::CONNECTION_CLOSED) { codes::UNAVAILABLE } else { error.code.as_str() };
            RpcError::new(code, format!("notes are kept by the memory plugin (memory@1): {method}: {}", error.message))
        })?;
        parse_params(value)
    }

    /// Import the JSON-lines notes of an earlier version once (retried on the next call when
    /// memory could not take them all).
    async fn ready(&self) -> Result<(), RpcError> {
        self.imported.get_or_try_init(|| self.import()).await.map(|_| ())
    }

    async fn import(&self) -> Result<(), RpcError> {
        let Ok(entries) = std::fs::read_dir(&self.legacy) else { return Ok(()) };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
        files.sort();
        for file in files {
            let text = std::fs::read_to_string(&file).map_err(|e| RpcError::internal(format!("{}: {e}", file.display())))?;
            // A torn last line (a crash mid-append) is skipped.
            for note in text.lines().filter_map(|line| serde_json::from_str::<Note>(line).ok()) {
                let params = memory::NoteAddParams {
                    project_id: None,
                    topic: note.topic,
                    claim: note.claim,
                    quote: note.quote,
                    url: note.url,
                    title: note.title,
                    date: note.date,
                    source: note.source,
                    question: note.question,
                    by: note.by,
                };
                self.memory::<_, memory::Note>(methods::NOTE_ADD, &params).await?;
            }
            let mut done = file.clone().into_os_string();
            done.push(".imported");
            std::fs::rename(&file, &done).map_err(|e| RpcError::internal(format!("{}: {e}", file.display())))?;
        }
        Ok(())
    }

    pub async fn add(&self, params: NoteAddParams) -> Result<Note, RpcError> {
        let required = [("topic", &params.topic), ("claim", &params.claim), ("quote", &params.quote), ("url", &params.url)];
        if let Some((name, _)) = required.iter().find(|(_, value)| value.trim().is_empty()) {
            return Err(RpcError::invalid_params(format!("a note needs `{name}`")));
        }
        self.ready().await?;
        let request = memory::NoteAddParams {
            project_id: scope(&params.project_id),
            topic: squash(&params.topic),
            claim: squash(&params.claim),
            quote: params.quote.trim().to_owned(),
            url: params.url.trim().to_owned(),
            title: squash(&params.title),
            date: params.date.as_deref().and_then(normalize_date).or(params.date.filter(|d| !d.trim().is_empty())),
            source: params.source,
            question: params.question,
            by: params.by,
        };
        Ok(note_from(self.memory(methods::NOTE_ADD, &request).await?))
    }

    /// Write one project knowledge item (`memory.knowledge.upsert`, docs/PROJECT-KNOWLEDGE-PLAN.md
    /// P5): what a deep research job found, as the project's `research`-sourced facts and open
    /// questions, each `SUPPORTED_BY` the notes it rests on. Returns the stored item.
    pub async fn knowledge_upsert(&self, params: memory::KnowledgeUpsertParams) -> Result<memory::KnowledgeItem, RpcError> {
        self.ready().await?;
        self.memory(methods::KNOWLEDGE_UPSERT, &params).await
    }

    /// A topic's notes (every topic's when None), oldest first.
    pub async fn list(&self, project_id: &str, topic: Option<&str>) -> Result<Vec<Note>, RpcError> {
        self.ready().await?;
        let request = memory::NoteListParams { project_id: scope(project_id), topic: topic.map(str::to_owned) };
        let result: memory::NoteListResult = self.memory(methods::NOTE_LIST, &request).await?;
        Ok(result.notes.into_iter().map(note_from).collect())
    }

    /// Whether memory answers note calls now: a list of a topic no note has.
    pub async fn check(&self) -> Result<(), RpcError> {
        let topic = format!("research probe {}", uuid::Uuid::new_v4().simple());
        let result: memory::NoteListResult = self.memory(methods::NOTE_LIST, &memory::NoteListParams { project_id: None, topic: Some(topic) }).await?;
        if result.notes.is_empty() { Ok(()) } else { Err(RpcError::internal("a new topic already has notes")) }
    }

    /// Best matches by meaning first.
    pub async fn search(&self, project_id: &str, topic: Option<&str>, text: &str, limit: u32) -> Result<Found, RpcError> {
        self.ready().await?;
        let request = memory::NoteSearchParams { project_id: scope(project_id), text: text.to_owned(), topic: topic.map(str::to_owned), limit };
        let result: memory::NoteSearchResult = self.memory(methods::NOTE_SEARCH, &request).await?;
        Ok(Found { notes: result.notes.into_iter().map(note_from).collect(), pending: result.pending })
    }
}
