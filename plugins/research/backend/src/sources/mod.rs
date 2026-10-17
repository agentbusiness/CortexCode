//! `research.search`: every source searched concurrently, each with its own parser (tested on
//! recorded responses), results interleaved across sources and deduplicated by URL.
//!
//! A source never invents a result: a source that cannot answer says why in its
//! [`SourceStatus::error`]; when no source could reach its server at all the whole search fails
//! with `unavailable` and a message starting with `offline:`.

pub mod arxiv;
pub mod github;
pub mod hackernews;
pub mod reddit;
pub mod semantic_scholar;
pub mod web;

use crate::net::{Net, NetError};
use crate::peers::Peers;
use cc_protocol::contracts::research::{SearchHit, SearchParams, SearchResult, SourceInfo, SourceStatus, sorts, sources};
use cc_protocol::{RpcError, codes};
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Relevance,
    Recent,
    Popular,
}

/// One search as a source sees it.
#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    /// `YYYY-MM-DD`.
    pub since: Option<String>,
    pub limit: u32,
    pub sort: Sort,
}

/// What a source found and what served it.
#[derive(Debug, Clone, Default)]
pub struct Found {
    pub hits: Vec<SearchHit>,
    pub via: String,
    /// A limitation worth saying even though the source answered (a fallback was used, ...).
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SourceError {
    Net(NetError),
    /// The source answered something that is not a result list (an API error, a changed page).
    Parse(String),
    /// The source needs something this machine does not have (gh sign-in, the browser).
    NotConfigured(String),
}

impl SourceError {
    pub fn message(&self) -> String {
        match self {
            Self::Net(error) => error.message(),
            Self::Parse(detail) => format!("unexpected answer: {detail}"),
            Self::NotConfigured(detail) => format!("not configured: {detail}"),
        }
    }

    fn unreachable(&self) -> bool {
        matches!(self, Self::Net(error) if error.is_unreachable())
    }
}

impl From<NetError> for SourceError {
    fn from(error: NetError) -> Self {
        Self::Net(error)
    }
}

/// Reads an environment variable (a key the user configured); replaceable in tests.
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct Engine {
    pub net: Arc<Net>,
    pub peers: Arc<dyn Peers>,
    pub env: EnvFn,
    /// Whether `gh` is installed and signed in, checked at most every five minutes.
    gh_ready: Mutex<Option<(Instant, bool)>>,
}

impl Engine {
    pub fn new(net: Arc<Net>, peers: Arc<dyn Peers>, env: EnvFn) -> Self {
        Self { net, peers, env, gh_ready: Mutex::new(None) }
    }

    pub fn config(&self) -> &crate::config::Config {
        self.net.config()
    }

    /// `gh auth status` succeeds (cached for five minutes).
    pub async fn gh_ready(&self) -> bool {
        if let Ok(cached) = self.gh_ready.lock()
            && let Some((at, ready)) = *cached
            && at.elapsed() < Duration::from_secs(300)
        {
            return ready;
        }
        let ready = github::gh_signed_in(&self.config().gh).await;
        if let Ok(mut cached) = self.gh_ready.lock() {
            *cached = Some((Instant::now(), ready));
        }
        ready
    }

    pub async fn search(&self, params: SearchParams, cancel: CancellationToken) -> Result<SearchResult, RpcError> {
        let query = params.query.trim().to_owned();
        if query.is_empty() {
            return Err(RpcError::invalid_params("research.search needs a query"));
        }
        let mut names: Vec<String> = if params.sources.is_empty() {
            sources::DEFAULT.iter().map(|s| (*s).to_owned()).collect()
        } else {
            params.sources.iter().map(|s| s.trim().to_ascii_lowercase().replace(['-', ' '], "_")).collect()
        };
        for name in &mut names {
            if name == "hn" {
                *name = sources::HACKERNEWS.to_owned();
            } else if name == "s2" || name == "semanticscholar" {
                *name = sources::SEMANTIC_SCHOLAR.to_owned();
            }
        }
        let mut unique = Vec::new();
        for name in names {
            if !unique.contains(&name) {
                unique.push(name);
            }
        }
        let names = unique;
        if let Some(unknown) = names.iter().find(|name| !sources::ALL.contains(&name.as_str())) {
            return Err(RpcError::invalid_params(format!("unknown source `{unknown}`; sources: {}", sources::ALL.join(", "))));
        }
        let since = match params.since.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(text) => Some(
                crate::text::normalize_date(text)
                    .ok_or_else(|| RpcError::invalid_params(format!("since `{text}` is not a date (YYYY-MM-DD)")))?,
            ),
        };
        let sort = match params.sort.as_deref().unwrap_or(sorts::RELEVANCE) {
            sorts::RELEVANCE | "" => Sort::Relevance,
            sorts::RECENT | "date" | "new" | "newest" => Sort::Recent,
            sorts::POPULAR | "stars" | "top" => Sort::Popular,
            other => return Err(RpcError::invalid_params(format!("sort `{other}`: relevance | recent | popular"))),
        };
        let q = Query { text: query.clone(), since, limit: params.limit.unwrap_or(5).clamp(1, 25), sort };
        let runs = names.iter().map(|name| {
            let q = q.clone();
            let cancel = cancel.clone();
            async move {
                let outcome = tokio::select! {
                    outcome = self.one(name, &q) => outcome,
                    _ = cancel.cancelled() => Err(SourceError::Parse("cancelled".into())),
                };
                (name.clone(), outcome)
            }
        });
        let outcomes = join_all(runs).await;
        if cancel.is_cancelled() {
            return Err(RpcError::cancelled());
        }
        let mut statuses = Vec::new();
        let mut lists = Vec::new();
        for (name, outcome) in &outcomes {
            match outcome {
                Ok(found) => {
                    let hits: Vec<SearchHit> = found.hits.iter().filter(|hit| after(hit, q.since.as_deref())).take(q.limit as usize).cloned().collect();
                    statuses.push(SourceStatus { source: name.clone(), ok: true, count: hits.len() as u32, via: found.via.clone(), error: found.note.clone() });
                    lists.push(hits);
                }
                Err(error) => statuses.push(SourceStatus { source: name.clone(), ok: false, count: 0, via: String::new(), error: Some(error.message()) }),
            }
        }
        if outcomes.iter().all(|(_, outcome)| outcome.as_ref().is_err_and(SourceError::unreachable)) {
            let details: Vec<String> = statuses.iter().map(|s| format!("{}: {}", s.source, s.error.clone().unwrap_or_default())).collect();
            return Err(RpcError::new(codes::UNAVAILABLE, format!("offline: no source could be reached ({})", details.join("; ")))
                .with_data(json!({ "offline": true, "sources": statuses })));
        }
        Ok(SearchResult { query, results: interleave(lists), sources: statuses })
    }

    async fn one(&self, name: &str, q: &Query) -> Result<Found, SourceError> {
        match name {
            sources::ARXIV => arxiv::search(self, q).await,
            sources::SEMANTIC_SCHOLAR => semantic_scholar::search(self, q).await,
            sources::GITHUB => github::search(self, q, github::Kind::Repos).await,
            sources::GITHUB_ISSUES => github::search(self, q, github::Kind::Issues).await,
            sources::GITHUB_CODE => github::search(self, q, github::Kind::Code).await,
            sources::REDDIT => reddit::search(self, q).await,
            sources::HACKERNEWS => hackernews::search(self, q).await,
            sources::WEB => web::search(self, q).await,
            other => Err(SourceError::NotConfigured(format!("unknown source {other}"))),
        }
    }

    /// What each source would use now (no network access).
    pub async fn describe(&self) -> Vec<SourceInfo> {
        let config = self.config();
        let key = |names: &[&str]| names.iter().any(|name| (self.env)(name).is_some_and(|v| !v.trim().is_empty()));
        let gh = config.github != "rest" && self.gh_ready().await;
        let token = key(&["GH_TOKEN", "GITHUB_TOKEN"]);
        let github_detail = if gh {
            "ready (gh, signed in)".to_owned()
        } else if token {
            "ready (REST with the token in GH_TOKEN / GITHUB_TOKEN)".to_owned()
        } else {
            "unauthenticated REST: about 10 searches a minute (`gh auth login` raises it)".to_owned()
        };
        let github_via = if gh { "gh api".to_owned() } else { config.github_api.clone() };
        vec![
            SourceInfo { source: sources::ARXIV.into(), via: config.arxiv_api.clone(), detail: format!("ready (one request every {} ms, as arXiv asks)", config.arxiv_interval_ms) },
            SourceInfo {
                source: sources::SEMANTIC_SCHOLAR.into(),
                via: config.semantic_scholar_api.clone(),
                detail: if key(&["SEMANTIC_SCHOLAR_API_KEY", "S2_API_KEY"]) {
                    "ready (your API key)".into()
                } else {
                    "ready (the shared free pool: often rate limited; SEMANTIC_SCHOLAR_API_KEY gives a personal limit)".into()
                },
            },
            SourceInfo { source: sources::GITHUB.into(), via: github_via.clone(), detail: github_detail.clone() },
            SourceInfo { source: sources::GITHUB_ISSUES.into(), via: github_via.clone(), detail: github_detail },
            SourceInfo {
                source: sources::GITHUB_CODE.into(),
                via: github_via,
                detail: if gh || token { "ready".into() } else { "needs `gh auth login` or GH_TOKEN (GitHub requires sign-in for code search)".into() },
            },
            SourceInfo { source: sources::REDDIT.into(), via: format!("{}/search.json, then search.rss", config.reddit), detail: "ready (JSON is often blocked without sign-in; RSS is the fallback)".into() },
            SourceInfo { source: sources::HACKERNEWS.into(), via: config.hackernews_api.clone(), detail: "ready".into() },
            SourceInfo {
                source: sources::WEB.into(),
                via: match config.web_search.as_str() {
                    "browser" => "the built-in browser (DuckDuckGo)".into(),
                    "http" => config.duckduckgo_html.clone(),
                    _ => format!("{} (the built-in browser when blocked)", config.duckduckgo_html),
                },
                detail: "ready (no CAPTCHA is ever solved: a blocked search says so)".into(),
            },
        ]
    }
}

/// Run futures concurrently and collect their outputs in order.
async fn join_all<F: std::future::Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut set = Vec::new();
    for future in futures {
        set.push(Box::pin(future));
    }
    let mut outputs: Vec<Option<F::Output>> = set.iter().map(|_| None).collect();
    let mut pending: Vec<usize> = (0..set.len()).collect();
    std::future::poll_fn(|cx| {
        pending.retain(|&index| match set[index].as_mut().poll(cx) {
            std::task::Poll::Ready(output) => {
                outputs[index] = Some(output);
                false
            }
            std::task::Poll::Pending => true,
        });
        if pending.is_empty() { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
    })
    .await;
    outputs.into_iter().flatten().collect()
}

/// Whether a hit is from `since` on (a hit without a date is kept).
fn after(hit: &SearchHit, since: Option<&str>) -> bool {
    match (since, hit.date.as_deref()) {
        (Some(since), Some(date)) => date >= since,
        _ => true,
    }
}

/// Round-robin across sources (each keeps its own order), dropping repeated URLs.
pub fn interleave(lists: Vec<Vec<SearchHit>>) -> Vec<SearchHit> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for rank in 0..longest {
        for list in &lists {
            if let Some(hit) = list.get(rank)
                && seen.insert(url_key(&hit.url))
            {
                out.push(hit.clone());
            }
        }
    }
    out
}

/// A URL for deduplication: no scheme, no `www.`, no trailing slash, no fragment.
pub fn url_key(url: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    let url = url.trim_start_matches("https://").trim_start_matches("http://").trim_start_matches("www.");
    url.trim_end_matches('/').to_ascii_lowercase()
}

/// `?a=b&c=d` with form encoding.
pub fn query_string(pairs: &[(&str, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

/// Days from `since` to today (for sources that filter by a period).
pub fn days_since(since: &str) -> Option<i64> {
    let date = chrono::NaiveDate::parse_from_str(since, "%Y-%m-%d").ok()?;
    Some((chrono::Utc::now().date_naive() - date).num_days())
}
