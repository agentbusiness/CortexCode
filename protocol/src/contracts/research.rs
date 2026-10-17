//! `research@1`: research that needs the world, online only when a network exists and without cloud
//! models, paid APIs or required keys.
//!
//! - **search** by source: arXiv and Semantic Scholar (papers, sortable by date), GitHub
//!   (repositories, issues, code; through `gh` when it is installed and signed in, else the public
//!   REST API), Reddit and Hacker News, and the web (a key-free search page, or the built-in browser);
//! - **fetch** a page or a PDF as clean readable text (cached under `$CORTEX_HOME/research/cache`);
//! - **notes**: cited findings (claim, verbatim quote, url, title, date) by topic, kept by the
//!   memory plugin in IronGraph's knowledge layer (`memory.note.*`): global unless a project id
//!   is given, searchable by meaning later;
//! - **deep** research as a background job: plan sub-questions, search, one model call per source
//!   (chunk) writing notes (map), synthesis from the notes only (reduce), a gap check that may run
//!   another round, and a report `docs/research/<topic-slug>.md` in the project.
//!
//! Without a network every network method fails plainly with `unavailable` and a message starting
//! with `offline:`; nothing is ever made up. Notes and the cache keep working offline; without the
//! memory plugin the note methods fail plainly with its error.

use serde::{Deserialize, Serialize};

pub const CONTRACT: &str = "research@1";

pub mod methods {
    /// [`super::SearchParams`] -> [`super::SearchResult`]. Searches the sources concurrently. With no
    /// network: `unavailable`, message `offline: ...`, data `{offline: true, sources: [SourceStatus]}`.
    pub const SEARCH: &str = "research.search";
    /// [`super::FetchParams`] -> [`super::FetchResult`]. A page through a readability extractor, a PDF
    /// through a PDF text extractor; cached by URL and content hash. Offline, a cached copy is
    /// returned (marked `cached` and `stale`); without one: `unavailable` `offline: ...`.
    pub const FETCH: &str = "research.fetch";
    /// [`super::NoteAddParams`] -> [`super::Note`]. Idempotent: the same scope (global or a
    /// project), topic, url and quote give the same note id.
    pub const NOTE_ADD: &str = "research.note.add";
    /// [`super::NoteSearchParams`] -> [`super::NotesResult`]: by meaning, best first, with `score`.
    pub const NOTE_SEARCH: &str = "research.note.search";
    /// [`super::NotesParams`] -> [`super::NotesResult`]: a topic's notes (every topic's when absent),
    /// oldest first.
    pub const NOTES: &str = "research.notes";
    /// [`super::DeepParams`] -> [`super::Job`] at once; the job runs in the background, publishing
    /// [`super::Progress`] on [`super::topics::PROGRESS`] and its [`super::Job`] on
    /// [`super::topics::JOB`] when its state changes. An unfinished job of the same project and topic
    /// is resumed (unless `fresh`); sources the topic's notes already cite are not read again.
    pub const DEEP: &str = "research.deep";
    /// [`super::JobParams`] -> [`super::Job`].
    pub const JOB_STATUS: &str = "research.job.status";
    /// [`super::JobParams`] -> [`super::Job`]: cancel a running job (its notes stay).
    pub const JOB_CANCEL: &str = "research.job.cancel";
    /// [`super::JobsParams`] -> [`super::JobsResult`], newest first.
    pub const JOBS: &str = "research.jobs";
    /// `{}` -> [`super::SourcesResult`]: what each source would use right now (gh signed in, keys,
    /// the browser), without network access.
    pub const SOURCES: &str = "research.sources";
}

pub mod topics {
    /// [`super::Progress`] of a deep research job.
    pub const PROGRESS: &str = "research.progress";
    /// [`super::Job`] whenever a job's state changes (running, done, failed, cancelled, interrupted).
    pub const JOB: &str = "research.job";
}

/// Source names ([`SearchParams::sources`], [`SearchHit::source`]).
pub mod sources {
    pub const WEB: &str = "web";
    pub const ARXIV: &str = "arxiv";
    pub const SEMANTIC_SCHOLAR: &str = "semantic_scholar";
    /// GitHub repositories.
    pub const GITHUB: &str = "github";
    /// GitHub issues and pull requests.
    pub const GITHUB_ISSUES: &str = "github_issues";
    /// GitHub code (needs `gh` signed in, or a token in `GH_TOKEN` / `GITHUB_TOKEN`).
    pub const GITHUB_CODE: &str = "github_code";
    pub const REDDIT: &str = "reddit";
    pub const HACKERNEWS: &str = "hackernews";
    /// Every source name.
    pub const ALL: &[&str] = &[WEB, ARXIV, SEMANTIC_SCHOLAR, GITHUB, GITHUB_ISSUES, GITHUB_CODE, REDDIT, HACKERNEWS];
    /// The sources searched when a search names none.
    pub const DEFAULT: &[&str] = &[ARXIV, SEMANTIC_SCHOLAR, GITHUB, HACKERNEWS, REDDIT, WEB];
}

/// [`SearchParams::sort`] values.
pub mod sorts {
    /// The source's own relevance order (default).
    pub const RELEVANCE: &str = "relevance";
    /// Newest first (arXiv submittedDate, GitHub updated, Reddit new, HN by date, ...).
    pub const RECENT: &str = "recent";
    /// Most popular first (GitHub stars, Reddit top, Semantic Scholar citations, ...).
    pub const POPULAR: &str = "popular";
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchParams {
    pub query: String,
    /// [`sources`] names; empty = [`sources::DEFAULT`].
    #[serde(default)]
    pub sources: Vec<String>,
    /// Only results from this date on (`YYYY-MM-DD`; a year `YYYY` or month `YYYY-MM` also works).
    #[serde(default)]
    pub since: Option<String>,
    /// Results per source (default 5, at most 25).
    #[serde(default)]
    pub limit: Option<u32>,
    /// [`sorts`]: `relevance` (default) | `recent` | `popular`.
    #[serde(default)]
    pub sort: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub source: String,
    pub title: String,
    pub url: String,
    /// `YYYY-MM-DD` (published, submitted, created or last updated, as the source reports).
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default)]
    pub snippet: String,
    /// The source's own popularity (GitHub stars, HN points, Reddit score, citations).
    #[serde(default)]
    pub score: Option<f64>,
    /// Source-specific details: `pdf`, `doi`, `arxiv_id`, `stars`, `language`, `comments`,
    /// `discussion` (the HN / Reddit thread when `url` links elsewhere), `subreddit`, `state`, ...
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// How one source answered.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SourceStatus {
    pub source: String,
    pub ok: bool,
    pub count: u32,
    /// What served it: `api`, `atom`, `gh`, `rest`, `json`, `rss`, `duckduckgo-html`, `browser`.
    #[serde(default)]
    pub via: String,
    /// Why it failed (or what limited it), in words: `offline: ...`, `rate limited: ...`,
    /// `blocked: ...`, `not configured: ...`.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    pub query: String,
    /// Interleaved across sources (each source's own order kept), deduplicated by URL.
    pub results: Vec<SearchHit>,
    pub sources: Vec<SourceStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FetchParams {
    pub url: String,
    /// Fetch again even when the cache holds a fresh copy.
    #[serde(default)]
    pub refresh: bool,
    /// Most characters of `text` returned (default: the plugin's `max_text_chars`).
    #[serde(default)]
    pub max_chars: Option<u64>,
    /// Characters of `text` to skip first (paging through a long document).
    #[serde(default)]
    pub offset: Option<u64>,
}

/// [`FetchResult::kind`] values.
pub mod kinds {
    pub const HTML: &str = "html";
    pub const PDF: &str = "pdf";
    /// Plain text, Markdown, JSON, XML, ... returned as they are.
    pub const OTHER: &str = "other";
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FetchResult {
    /// The URL asked for.
    pub url: String,
    pub title: String,
    /// Clean readable text (paragraphs separated by blank lines).
    pub text: String,
    /// [`kinds`]: `html` | `pdf` | `other`.
    pub kind: String,
    /// `YYYY-MM-DD` when the page or PDF says when it was published.
    #[serde(default)]
    pub published: Option<String>,
    /// Bytes downloaded.
    pub bytes: u64,
    /// The text was cut (download limit, `max_chars`, or the extractor's limit).
    pub truncated: bool,
    /// Where the URL led after redirects (when different).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    /// PDF pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    /// Characters of the whole extracted text (before `offset` / `max_chars`).
    #[serde(default)]
    pub total_chars: u64,
    /// Served from the cache.
    #[serde(default)]
    pub cached: bool,
    /// A cached copy older than the cache lifetime, returned because the network is not reachable.
    #[serde(default)]
    pub stale: bool,
    /// When it was downloaded (ms since the Unix epoch).
    #[serde(default)]
    pub fetched_ms: u64,
    /// SHA-256 of the downloaded bytes.
    #[serde(default)]
    pub content_sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteAddParams {
    /// Empty: a global note (what research writes); a project id keeps it to that project.
    #[serde(default)]
    pub project_id: String,
    pub topic: String,
    /// The finding, in the note writer's words.
    pub claim: String,
    /// Verbatim text from the source that supports the claim.
    pub quote: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// The source's date (`YYYY-MM-DD`).
    #[serde(default)]
    pub date: Option<String>,
    /// Which search source found it (`arxiv`, `web`, ...).
    #[serde(default)]
    pub source: Option<String>,
    /// The sub-question it answers.
    #[serde(default)]
    pub question: Option<String>,
    /// Who wrote it: `user`, `model`, a job id.
    #[serde(default)]
    pub by: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Note {
    /// `n-<16 hex>`, derived from scope, topic, url and quote.
    pub id: String,
    /// Empty: a global note.
    #[serde(default)]
    pub project_id: String,
    pub topic: String,
    pub claim: String,
    pub quote: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
    pub created_ms: u64,
    /// Relevance in a search result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NoteSearchParams {
    /// Empty: the global notes.
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub topic: Option<String>,
    pub text: String,
    /// Default 20.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NotesParams {
    /// Empty: the global notes.
    #[serde(default)]
    pub project_id: String,
    /// Absent: every topic.
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NotesResult {
    pub notes: Vec<Note>,
    /// A search: notes in scope that IronGraph has not embedded yet (not ranked by it).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pending: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeepParams {
    pub project_id: String,
    pub topic: String,
    /// Questions to answer; empty: the model plans them from the topic.
    #[serde(default)]
    pub questions: Vec<String>,
    /// Rounds of search -> read -> synthesize -> gap check (default 2, at most 4).
    #[serde(default)]
    pub depth: Option<u32>,
    /// Sources to search (default: the planner's choice among [`sources::DEFAULT`]).
    #[serde(default)]
    pub sources: Vec<String>,
    /// Only sources from this date on (`YYYY-MM-DD`).
    #[serde(default)]
    pub since: Option<String>,
    /// Sources read per round (default: the plugin's `deep_sources_per_round`).
    #[serde(default)]
    pub max_sources: Option<u32>,
    /// The project root the report is written under (default: the frame's project root).
    #[serde(default)]
    pub root: Option<String>,
    /// Start over instead of resuming an unfinished job of the same topic.
    #[serde(default)]
    pub fresh: bool,
    /// The session that asked (carried in progress events).
    #[serde(default)]
    pub session_id: Option<String>,
}

/// [`Job::state`] values.
pub mod states {
    pub const RUNNING: &str = "running";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
    /// The plugin stopped while the job ran; `research.deep` on the topic resumes it.
    pub const INTERRUPTED: &str = "interrupted";
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub job_id: String,
    pub project_id: String,
    pub topic: String,
    /// [`states`]
    pub state: String,
    /// plan | search | read | synthesize | gap | report | done
    pub phase: String,
    /// 1-based round; `depth` rounds at most.
    pub round: u32,
    pub depth: u32,
    pub questions: Vec<String>,
    /// Sources read (fetched and passed through the model) so far, all rounds.
    pub sources_read: u32,
    /// Sources that could not be fetched or read.
    pub sources_failed: u32,
    /// Notes of the topic (all runs, all projects: research notes are global).
    pub notes: u32,
    /// The report, once written (absolute path).
    #[serde(default)]
    pub report_path: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// How many times it was resumed.
    #[serde(default)]
    pub resumed: u32,
    pub started_ms: u64,
    pub updated_ms: u64,
}

/// One progress step of a deep research job.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    pub job_id: String,
    pub project_id: String,
    pub topic: String,
    /// plan | search | read | synthesize | gap | report | done | failed | cancelled
    pub phase: String,
    pub round: u32,
    /// Units done and planned in this phase (sources read of those picked, ...), when countable.
    #[serde(default)]
    pub done: u32,
    #[serde(default)]
    pub total: u32,
    pub detail: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobParams {
    pub job_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobsParams {
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobsResult {
    pub jobs: Vec<Job>,
}

/// What a source would use now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SourceInfo {
    pub source: String,
    /// The endpoint or tool it uses.
    pub via: String,
    /// `ready`, or why it is limited (`unauthenticated: 10 searches a minute`, ...).
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SourcesResult {
    pub sources: Vec<SourceInfo>,
}
