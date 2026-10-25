//! `research://` URIs: the research plugin (`research@1`) as things the model reads.
//!
//! - `research://search/<query>?sources=arxiv,github&since=2025-01-01&limit=5&sort=recent`
//! - `research://fetch/<url>` (the rest of the URI is the URL, query string included); a long
//!   document continues with `research://fetch/<url>#from=<character offset>`
//! - `research://notes/<topic>` (the research notes on a topic, global: kept by the memory plugin;
//!   `research://notes/` for all) and `research://notes/<topic>?q=<words>` (searched by meaning)
//!
//! Without a network the research plugin answers `offline: ...`, which is passed on as it is. An
//! offline workspace (`.cortex-offline`, see `sandbox`) gets the same answer for search and fetch
//! without the research plugin being asked: offline is the workspace's property, so nothing global
//! (the plugin set) is switched for it. Notes are local and stay readable.

use crate::host::Host;
use crate::uris::UriError;
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Characters of a fetched document per read.
const PAGE_CHARS: u64 = 12_000;

fn error(kind: &'static str, message: impl Into<String>) -> UriError {
    UriError { kind, message: message.into() }
}

fn rpc(error_value: RpcError) -> UriError {
    match error_value.code.as_str() {
        codes::UNKNOWN_METHOD | codes::CONNECTION_CLOSED => error("unavailable", format!("the research plugin is not available ({})", error_value.message)),
        codes::UNAVAILABLE => error("unavailable", error_value.message),
        codes::INVALID_PARAMS => error("bad_uri", error_value.message),
        codes::NOT_FOUND => error("not_found", error_value.message),
        _ => error("uri_error", format!("research: {}", error_value.message)),
    }
}

fn decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text).decode_utf8_lossy().into_owned()
}

fn query_params(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(&value.replace('+', " ")))
        })
        .collect()
}

const HELP: &str = "research:// reads: research://search/<query>?sources=arxiv,semantic_scholar,github,github_issues,github_code,reddit,hackernews,web&since=YYYY-MM-DD&limit=N&sort=relevance|recent|popular, research://fetch/<url> (#from=<offset> continues a long document), research://notes/<topic>[?q=<words>]";

/// Read one `research://` URI.
pub async fn read(host: &dyn Host, uri: &str, offline: bool) -> Result<String, UriError> {
    let rest = uri.strip_prefix("research://").ok_or_else(|| error("bad_uri", HELP))?;
    let (verb, rest) = rest.split_once('/').unwrap_or((rest, ""));
    if offline && matches!(verb, "search" | "fetch") {
        return Err(error("unavailable", "offline: this project has no network, and no route reaches the internet here: not research://search or research://fetch, not curl or git, not a download or an install. Work from the project's files."));
    }
    match verb {
        "search" => search(host, rest).await,
        "fetch" => fetch(host, rest).await,
        "notes" | "note" => notes(host, rest).await,
        _ => Err(error("bad_uri", HELP)),
    }
}

async fn search(host: &dyn Host, rest: &str) -> Result<String, UriError> {
    let (query, params) = match rest.split_once('?') {
        Some((query, params)) => (decode(query), query_params(params)),
        None => (decode(rest), HashMap::new()),
    };
    if query.trim().is_empty() {
        return Err(error("bad_uri", "research://search/<query> needs a query"));
    }
    let mut request = json!({ "query": query });
    if let Some(sources) = params.get("sources").or_else(|| params.get("source")) {
        request["sources"] = json!(sources.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>());
    }
    for key in ["since", "sort"] {
        if let Some(value) = params.get(key) {
            request[key] = json!(value);
        }
    }
    if let Some(limit) = params.get("limit").and_then(|l| l.parse::<u64>().ok()) {
        request["limit"] = json!(limit);
    }
    let result = host.call("research.search", request).await.map_err(rpc)?;
    Ok(render_search(&query, &result))
}

fn render_search(query: &str, result: &Value) -> String {
    let s = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let hits = result.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = String::new();
    if hits.is_empty() {
        let _ = writeln!(out, "Nothing found for `{query}`.");
    }
    for (i, hit) in hits.iter().enumerate() {
        let date = hit.get("date").and_then(Value::as_str).map(|d| format!(" ({d})")).unwrap_or_default();
        let authors: Vec<String> = hit.get("authors").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(3).map(str::to_owned).collect()).unwrap_or_default();
        let by = if authors.is_empty() { String::new() } else { format!(" — {}", authors.join(", ")) };
        let _ = writeln!(out, "{}. [{}] {}{date}{by}\n   {}", i + 1, s(hit, "source"), s(hit, "title"), s(hit, "url"));
        let snippet = s(hit, "snippet");
        if !snippet.is_empty() {
            let short: String = snippet.chars().take(240).collect();
            let _ = writeln!(out, "   {short}{}", if snippet.chars().count() > 240 { "…" } else { "" });
        }
        if let Some(extra) = hit.get("extra") {
            let discussion = s(extra, "discussion");
            if !discussion.is_empty() && discussion != s(hit, "url") {
                let _ = writeln!(out, "   discussion: {discussion}");
            }
            let pdf = s(extra, "pdf");
            if !pdf.is_empty() {
                let _ = writeln!(out, "   pdf: {pdf}");
            }
        }
    }
    // The sources' own states only when none of them answered: then the empty list is not news.
    let sources = result.get("sources").and_then(Value::as_array).cloned().unwrap_or_default();
    if !sources.is_empty() && sources.iter().all(|st| st.get("ok").and_then(Value::as_bool) != Some(true)) {
        let failed: Vec<String> = sources
            .iter()
            .map(|st| match st.get("error").and_then(Value::as_str) {
                Some(error) => format!("{}: {error}", s(st, "source")),
                None => s(st, "source"),
            })
            .collect();
        let _ = writeln!(out, "Every source failed: {}", failed.join("; "));
    }
    if !hits.is_empty() {
        out.push_str("Read a result with research://fetch/<url>.");
    }
    out.trim_end().to_owned()
}

async fn fetch(host: &dyn Host, rest: &str) -> Result<String, UriError> {
    let (url, from) = match rest.rsplit_once("#from=") {
        Some((url, offset)) => (url.to_owned(), offset.trim().parse::<u64>().map_err(|_| error("bad_uri", "#from= needs a character offset"))?),
        None => (rest.to_owned(), 0),
    };
    let url = if url.starts_with("http%3A") || url.starts_with("https%3A") { decode(&url) } else { url };
    if url.trim().is_empty() {
        return Err(error("bad_uri", "research://fetch/<url> needs a URL, e.g. research://fetch/https://arxiv.org/abs/2211.17192"));
    }
    let result = host.call("research.fetch", json!({ "url": url, "offset": from, "max_chars": PAGE_CHARS })).await.map_err(rpc)?;
    Ok(render_fetch(&url, from, &result))
}

fn render_fetch(url: &str, from: u64, result: &Value) -> String {
    let s = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let n = |key: &str| result.get(key).and_then(Value::as_u64).unwrap_or(0);
    let text = s("text");
    let mut facts = vec![s("kind")];
    if let Some(date) = result.get("published").and_then(Value::as_str) {
        facts.push(format!("published {date}"));
    }
    if let Some(pages) = result.get("pages").and_then(Value::as_u64) {
        facts.push(format!("{pages} pages"));
    }
    facts.push(format!("{} chars", n("total_chars")));
    if result.get("stale").and_then(Value::as_bool) == Some(true) {
        facts.push("cached copy (offline)".into());
    } else if result.get("cached").and_then(Value::as_bool) == Some(true) {
        facts.push("cached".into());
    }
    let shown_to = from + text.chars().count() as u64;
    let mut out = format!("{}\n{} ({})\n", s("title"), result.get("final_url").and_then(Value::as_str).unwrap_or(url), facts.join(", "));
    if from > 0 {
        let _ = writeln!(out, "[from character {from}]");
    }
    out.push_str("---\n");
    out.push_str(&text);
    if shown_to < n("total_chars") {
        let _ = write!(out, "\n---\n[{shown_to} of {} characters; continue with research://fetch/{url}#from={shown_to}]", n("total_chars"));
    }
    out
}

async fn notes(host: &dyn Host, rest: &str) -> Result<String, UriError> {
    let (topic, params) = match rest.split_once('?') {
        Some((topic, params)) => (decode(topic), query_params(params)),
        None => (decode(rest), HashMap::new()),
    };
    let topic = topic.trim().trim_matches('/').to_owned();
    let mut request = json!({});
    if !topic.is_empty() {
        request["topic"] = json!(topic);
    }
    let result = match params.get("q").filter(|q| !q.trim().is_empty()) {
        Some(text) => {
            request["text"] = json!(text);
            host.call("research.note.search", request).await
        }
        None => host.call("research.notes", request).await,
    }
    .map_err(rpc)?;
    let notes = result.get("notes").and_then(Value::as_array).cloned().unwrap_or_default();
    if notes.is_empty() {
        return Ok(format!("no research notes{}", if topic.is_empty() { String::new() } else { format!(" on `{topic}`") }));
    }
    let mut out = String::new();
    for note in &notes {
        let s = |key: &str| note.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
        let date = note.get("date").and_then(Value::as_str).map(|d| format!(", {d}")).unwrap_or_default();
        let _ = writeln!(out, "- {} [{}] {}\n  \"{}\" — {}{date} <{}>", s("id"), s("topic"), s("claim"), s("quote"), s("title"), s("url"));
    }
    Ok(out.trim_end().to_owned())
}
