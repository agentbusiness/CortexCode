//! `research.fetch`: download (size-limited, private addresses refused), extract readable text and
//! keep both:
//!
//! ```text
//! (:WebPage {key: url, url, final_url, fetched_ms, content_sha256, content_type, kind, bytes,
//!     download_truncated, extractor, title, text, published, authors, site_name, pages})-[:ON]->(:Machine)
//! ```
//!
//! - the URL's latest fetch (when, the content hash, the extracted text and metadata, the extractor
//!   version) as a `WebPage` node in the memory plugin's graph (workspace layer); a URL longer than
//!   a key holds is keyed `sha256:<hex of the URL>`;
//! - the downloaded bytes stay a file, `$CORTEX_HOME/research/cache/blobs/<sha256 of the content>`,
//!   shared by every URL with the same content, so a newer extractor re-extracts without the network.
//!
//! A fresh entry (younger than `cache_ttl_hours`) is served without the network; an older one is
//! fetched again, and returned marked `stale` when the network cannot be reached. While memory is
//! away a lookup is a miss and a new entry is written once it serves.
//!
//! Entries an earlier version kept as files (`cache/urls/<sha256 of the URL>.json`) are written to
//! the graph once and the files deleted.

use crate::config::Config;
use crate::extract::{self, EXTRACTOR_VERSION, Extracted};
use crate::graph::{self, BRIEF, Graph, PATIENT, edge, gref, node};
use crate::net::{Net, NetError, Request};
use crate::text::{sha256_hex, skip_chars, take_chars};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::research::{FetchParams, FetchResult};
use cc_protocol::{RpcError, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PAGE: &str = "WebPage";
/// Entries moved per put, and their text at most (bytes), when moving an earlier version's files.
const MOVE_ENTRIES: usize = 200;
const MOVE_BYTES: usize = 16 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    url: String,
    final_url: String,
    fetched_ms: u64,
    content_sha256: String,
    content_type: String,
    kind: String,
    bytes: u64,
    download_truncated: bool,
    extractor: u32,
    title: String,
    text: String,
    #[serde(default)]
    published: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    site_name: Option<String>,
    #[serde(default)]
    pages: Option<u32>,
}

impl Entry {
    fn props(&self) -> Map<String, Value> {
        let value = json!({
            "url": self.url,
            "final_url": self.final_url,
            "fetched_ms": self.fetched_ms,
            "content_sha256": self.content_sha256,
            "content_type": self.content_type,
            "kind": self.kind,
            "bytes": self.bytes,
            "download_truncated": self.download_truncated,
            "extractor": self.extractor,
            "title": self.title,
            "text": self.text,
            "published": graph::or_null(&self.published),
            "authors": self.authors,
            "site_name": graph::or_null(&self.site_name),
            "pages": graph::or_null(&self.pages),
        });
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    fn from_props(p: &Map<String, Value>) -> Entry {
        Entry {
            url: graph::text(p, "url"),
            final_url: graph::text(p, "final_url"),
            fetched_ms: graph::number(p, "fetched_ms"),
            content_sha256: graph::text(p, "content_sha256"),
            content_type: graph::text(p, "content_type"),
            kind: graph::text(p, "kind"),
            bytes: graph::number(p, "bytes"),
            download_truncated: graph::flag(p, "download_truncated"),
            extractor: graph::number(p, "extractor") as u32,
            title: graph::text(p, "title"),
            text: graph::text(p, "text"),
            published: graph::opt_text(p, "published"),
            authors: graph::texts(p, "authors"),
            site_name: graph::opt_text(p, "site_name"),
            pages: graph::opt_number(p, "pages").map(|n| n as u32),
        }
    }
}

pub struct Fetcher {
    net: Arc<Net>,
    graph: Arc<Graph>,
    dir: PathBuf,
}

impl Fetcher {
    /// `dir`: `$CORTEX_HOME/research/cache` (the downloaded bytes, and an earlier version's entries).
    pub fn new(net: Arc<Net>, graph: Arc<Graph>, dir: PathBuf) -> Self {
        Self { net, graph, dir }
    }

    fn config(&self) -> &Config {
        self.net.config()
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.dir.join("blobs").join(hash)
    }

    /// The URL's entry; none while memory is away (a miss, fetched again).
    async fn load(&self, url: &str) -> Option<Entry> {
        let rows = self
            .graph
            .query(&format!("MATCH (w:{PAGE}) WHERE w.key = $key AND w.ended_ms IS NULL RETURN w"), json!({ "key": graph::fit_key(url) }), 1, BRIEF)
            .await
            .map_err(|error| tracing::info!("research cache: no lookup of {url}: {}", error.message))
            .ok()?;
        let entry = Entry::from_props(&graph::props_of(rows.first()?.first()?));
        (!entry.content_sha256.is_empty()).then_some(entry)
    }

    fn store(&self, entry: &Entry, body: &[u8]) -> Result<(), String> {
        write_atomic(&self.blob_path(&entry.content_sha256), body)?;
        self.store_entry(entry);
        Ok(())
    }

    /// Write the entry to the graph in the background: when memory is away, once it serves.
    fn store_entry(&self, entry: &Entry) {
        let (graph, key, props, url) = (self.graph.clone(), graph::fit_key(&entry.url), entry.props(), entry.url.clone());
        tokio::spawn(async move {
            let written = async {
                let machine = graph.machine().await?;
                graph.put(vec![node(PAGE, &key, props)], vec![edge(gref(PAGE, &key), "ON", gref("Machine", &machine))], PATIENT).await
            };
            if let Err(error) = written.await {
                tracing::warn!("research cache: cannot keep {url}: {}", error.message);
            }
        });
    }

    /// Move the entries an earlier version kept as files (`urls/*.json`) into the graph: a put per
    /// group, each file deleted once its group is written. Files left by a failed put are moved on
    /// the next start.
    pub async fn migrate(&self) {
        let dir = self.dir.join("urls");
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut group: Vec<(PathBuf, Entry)> = Vec::new();
        let mut size = 0;
        for path in entries.flatten().map(|e| e.path()) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.ends_with(".tmp") {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if !name.ends_with(".json") {
                continue;
            }
            // An entry that does not read is a cache miss: the URL is fetched again when asked.
            let Some(entry) = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Entry>(&text).ok()) else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            size += entry.text.len();
            group.push((path, entry));
            if group.len() >= MOVE_ENTRIES || size >= MOVE_BYTES {
                if !self.move_group(std::mem::take(&mut group)).await {
                    return;
                }
                size = 0;
            }
        }
        if !group.is_empty() && !self.move_group(group).await {
            return;
        }
        let _ = std::fs::remove_dir(&dir);
    }

    async fn move_group(&self, group: Vec<(PathBuf, Entry)>) -> bool {
        let written = async {
            let machine = gref("Machine", &self.graph.machine().await?);
            let mut nodes = Vec::new();
            let mut edges = Vec::new();
            for (_, entry) in &group {
                let key = graph::fit_key(&entry.url);
                nodes.push(node(PAGE, &key, entry.props()));
                edges.push(edge(gref(PAGE, &key), "ON", machine.clone()));
            }
            self.graph.put(nodes, edges, PATIENT).await
        };
        match written.await {
            Ok(()) => {
                for (path, _) in group {
                    let _ = std::fs::remove_file(&path);
                }
                true
            }
            Err(error) => {
                tracing::warn!("research cache: cannot move the cached pages into the graph ({}); they move on the next start", error.message);
                false
            }
        }
    }

    pub async fn fetch(&self, params: FetchParams) -> Result<FetchResult, RpcError> {
        let url = params.url.trim().split('#').next().unwrap_or_default().to_owned();
        if url.is_empty() {
            return Err(RpcError::invalid_params("research.fetch needs a url"));
        }
        let parsed = url::Url::parse(&url).map_err(|error| RpcError::invalid_params(format!("`{url}` is not a URL ({error})")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(RpcError::invalid_params(format!("only http and https URLs are fetched, not {}:", parsed.scheme())));
        }
        let ttl_ms = self.config().cache_ttl_hours.saturating_mul(3_600_000);
        let cached = self.load(&url).await;
        if !params.refresh
            && let Some(entry) = &cached
            && now_ms().saturating_sub(entry.fetched_ms) < ttl_ms
        {
            let entry = self.reextract(entry.clone()).await;
            return Ok(result(&entry, &params, true, false, self.config().max_text_chars));
        }
        match self.download(&url).await {
            Ok(entry) => Ok(result(&entry, &params, false, false, self.config().max_text_chars)),
            Err(error) => match (&error, cached) {
                (FetchError::Net(net), Some(entry)) if net.is_unreachable() => {
                    let entry = self.reextract(entry).await;
                    Ok(result(&entry, &params, true, true, self.config().max_text_chars))
                }
                _ => Err(error.into_rpc(&url)),
            },
        }
    }

    /// An entry extracted by an older extractor is extracted again from its stored bytes.
    async fn reextract(&self, entry: Entry) -> Entry {
        if entry.extractor == EXTRACTOR_VERSION {
            return entry;
        }
        let Ok(body) = std::fs::read(self.blob_path(&entry.content_sha256)) else { return entry };
        let limit = self.config().max_text_chars;
        let (kind, final_url, content_type) = (entry.kind.clone(), entry.final_url.clone(), entry.content_type.clone());
        let extracted = tokio::task::spawn_blocking(move || run_extract(&kind, &body, &content_type, &final_url)).await;
        match extracted {
            Ok(Ok(extracted)) => {
                let fresh = fill(entry.clone(), extracted, limit);
                self.store_entry(&fresh);
                fresh
            }
            _ => entry,
        }
    }

    async fn download(&self, url: &str) -> Result<Entry, FetchError> {
        let mut target = url.to_owned();
        let mut hops = 0;
        loop {
            let response = self
                .net
                .get(Request { guard: true, ..Request::get(&target) }.header("accept", "text/html,application/xhtml+xml,application/pdf;q=0.9,text/plain;q=0.8,*/*;q=0.5"))
                .await
                .map_err(FetchError::Net)?;
            let kind = extract::kind_of(&response.content_type, &response.body)
                .ok_or_else(|| FetchError::Unsupported(format!("{} is {} (not a page, a PDF or text)", target, if response.content_type.is_empty() { "binary" } else { &response.content_type })))?;
            if kind == "pdf" && response.truncated {
                return Err(FetchError::Unsupported(format!(
                    "the PDF is larger than max_download_bytes ({} bytes); raise the limit to read it",
                    self.config().max_download_bytes
                )));
            }
            let body = response.body.clone();
            let (content_type, final_url) = (response.content_type.clone(), response.url.clone());
            let kind_owned = kind.to_owned();
            let (body_for_extract, ct, fu) = (body.clone(), content_type.clone(), final_url.clone());
            let extracted = tokio::task::spawn_blocking(move || run_extract(&kind_owned, &body_for_extract, &ct, &fu))
                .await
                .map_err(|error| FetchError::Extract(format!("the extractor stopped: {error}")))?
                .map_err(FetchError::Extract)?;
            if let Some(next) = &extracted.redirect
                && hops < 2
            {
                hops += 1;
                target = next.clone();
                continue;
            }
            let entry = fill(
                Entry {
                    url: url.to_owned(),
                    final_url,
                    fetched_ms: now_ms(),
                    content_sha256: sha256_hex(&body),
                    content_type,
                    kind: kind.to_owned(),
                    bytes: body.len() as u64,
                    download_truncated: response.truncated,
                    extractor: EXTRACTOR_VERSION,
                    title: String::new(),
                    text: String::new(),
                    published: None,
                    authors: Vec::new(),
                    site_name: None,
                    pages: None,
                },
                extracted,
                self.config().max_text_chars,
            );
            if let Err(error) = self.store(&entry, &body) {
                tracing::warn!("research cache: cannot store {url}: {error}");
            }
            return Ok(entry);
        }
    }
}

fn run_extract(kind: &str, body: &[u8], content_type: &str, url: &str) -> Result<Extracted, String> {
    match kind {
        "pdf" => extract::pdf(body),
        "html" => Ok(extract::html(&crate::net::decode_text(body, content_type), url)),
        _ => Ok(extract::other(&crate::net::decode_text(body, content_type), url)),
    }
}

fn fill(mut entry: Entry, extracted: Extracted, max_chars: u64) -> Entry {
    let (text, cut) = take_chars(&extracted.text, max_chars as usize);
    entry.text = text.to_owned();
    entry.download_truncated |= cut;
    entry.title = if extracted.title.is_empty() { entry.final_url.clone() } else { extracted.title };
    entry.published = extracted.published;
    entry.authors = extracted.authors;
    entry.site_name = extracted.site_name;
    entry.pages = extracted.pages;
    entry.extractor = EXTRACTOR_VERSION;
    entry
}

fn result(entry: &Entry, params: &FetchParams, cached: bool, stale: bool, default_max: u64) -> FetchResult {
    let total_chars = entry.text.chars().count() as u64;
    let rest = skip_chars(&entry.text, params.offset.unwrap_or(0) as usize);
    let (text, cut) = take_chars(rest, params.max_chars.unwrap_or(default_max) as usize);
    FetchResult {
        url: entry.url.clone(),
        title: entry.title.clone(),
        text: text.to_owned(),
        kind: entry.kind.clone(),
        published: entry.published.clone(),
        bytes: entry.bytes,
        truncated: entry.download_truncated || cut,
        final_url: (entry.final_url != entry.url).then(|| entry.final_url.clone()),
        authors: entry.authors.clone(),
        site_name: entry.site_name.clone(),
        pages: entry.pages,
        total_chars,
        cached,
        stale,
        fetched_ms: entry.fetched_ms,
        content_sha256: entry.content_sha256.clone(),
    }
}

#[derive(Debug)]
enum FetchError {
    Net(NetError),
    Unsupported(String),
    Extract(String),
}

impl FetchError {
    fn into_rpc(self, url: &str) -> RpcError {
        match self {
            FetchError::Net(error) if error.is_unreachable() => {
                RpcError::new(codes::UNAVAILABLE, format!("offline or unreachable: {} (no cached copy of {url})", error.message()))
                    .with_data(serde_json::json!({ "offline": true }))
            }
            FetchError::Net(NetError::Refused(detail)) => RpcError::invalid_params(format!("refused: {detail}")),
            FetchError::Net(NetError::Status(404, detail)) => RpcError::not_found(format!("{url}: not found ({detail})")),
            FetchError::Net(error) => RpcError::new(codes::UNAVAILABLE, format!("{url}: {}", error.message())),
            FetchError::Unsupported(detail) => RpcError::invalid_params(detail),
            FetchError::Extract(detail) => RpcError::internal(format!("{url}: {detail}")),
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let tmp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}
