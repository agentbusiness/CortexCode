//! Web search without keys. `web_search = "auto"` (default): DuckDuckGo's HTML endpoint
//! (`html.duckduckgo.com/html/`, a plain page of results made for clients without JavaScript) over
//! HTTP; when it answers with its bot check (HTTP 202/403, the "anomaly" page) or something that is
//! not a result page, the same search is opened in the built-in browser (a hidden background tab of
//! the `browser` plugin: a real Chromium with the user's persistent profile) and the result links are
//! read from its DOM. `http` and `browser` use only one of the two. A CAPTCHA is never solved: a
//! blocked search is reported as blocked.
//!
//! Why this order: the HTML endpoint needs no window, is fast and is stable markup; the browser is
//! the more robust path against bot checks but exists only while the CortexCode window runs and
//! costs a page load.

use super::{Engine, Found, Query, SourceError, days_since, query_string};
use crate::net::{NetError, Request};
use crate::text::{clip, squash};
use cc_protocol::contracts::research::{SearchHit, sources};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub fn url(base: &str, q: &Query) -> String {
    let mut pairs = vec![("q", q.text.trim().to_owned())];
    // DuckDuckGo's period filter: past day, week, month, year.
    let period = match q.since.as_deref().and_then(days_since) {
        Some(days) if days <= 1 => Some("d"),
        Some(days) if days <= 7 => Some("w"),
        Some(days) if days <= 31 => Some("m"),
        Some(days) if days <= 365 => Some("y"),
        _ => None,
    };
    if let Some(period) = period {
        pairs.push(("df", period.to_owned()));
    }
    format!("{base}?{}", query_string(&pairs))
}

/// What a result page held.
#[derive(Debug, Clone, PartialEq)]
pub enum Page {
    Results(Vec<SearchHit>),
    /// A bot check or CAPTCHA.
    Blocked(String),
    /// Neither results, nor "no results", nor a known block page.
    Unknown(String),
}

/// The target of a DuckDuckGo redirect link (`//duckduckgo.com/l/?uddg=<encoded>&rut=...`), or the
/// link itself.
pub fn unwrap_link(href: &str) -> Option<String> {
    let absolute = if let Some(rest) = href.strip_prefix("//") { format!("https://{rest}") } else { href.to_owned() };
    let parsed = url::Url::parse(&absolute).ok()?;
    if parsed.host_str().is_some_and(|h| h.ends_with("duckduckgo.com")) {
        if parsed.path() == "/l/" {
            return parsed.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.into_owned());
        }
        // An ad (`/y.js?...`) or an internal link.
        return None;
    }
    Some(absolute)
}

const BLOCK_MARKERS: &[&str] = &["anomaly-modal", "bots use duckduckgo", "captcha", "unusual traffic", "challenge-form", "are you a robot"];

/// Parse DuckDuckGo's HTML result page.
pub fn parse(html: &str) -> Page {
    let doc = dom_query::Document::from(html);
    let mut hits = Vec::new();
    for result in doc.select(".result").iter() {
        let class = result.attr("class").map(|c| c.to_string()).unwrap_or_default();
        if class.contains("result--ad") {
            continue;
        }
        let anchor = result.select("a.result__a");
        let Some(href) = anchor.attr("href") else { continue };
        let Some(target) = unwrap_link(&href) else { continue };
        let title = squash(&anchor.text());
        if title.is_empty() {
            continue;
        }
        let snippet = squash(&result.select(".result__snippet").text());
        hits.push(SearchHit { source: sources::WEB.into(), title, url: target, snippet: clip(&snippet, 400), ..SearchHit::default() });
    }
    if !hits.is_empty() {
        return Page::Results(hits);
    }
    let lower = html.to_lowercase();
    if let Some(marker) = BLOCK_MARKERS.iter().find(|m| lower.contains(*m)) {
        return Page::Blocked(format!("DuckDuckGo showed its bot check ({marker})"));
    }
    if doc.select(".no-results").exists() || lower.contains("no results.") || lower.contains("no  results") {
        return Page::Results(Vec::new());
    }
    Page::Unknown(clip(&squash(&doc.select("title").text()), 80))
}

pub async fn search(engine: &Engine, q: &Query) -> Result<Found, SourceError> {
    let mode = engine.config().web_search.clone();
    if mode == "browser" {
        return via_browser(engine, q).await;
    }
    let http = via_http(engine, q).await;
    match (mode.as_str(), http) {
        (_, Ok(found)) => Ok(found),
        ("http", Err(error)) => Err(error),
        (_, Err(error)) if error_is_final(&error) => Err(error),
        (_, Err(error)) => match via_browser(engine, q).await {
            Ok(mut found) => {
                found.note = Some(format!("the HTML endpoint failed ({}); used the built-in browser", error.message()));
                Ok(found)
            }
            Err(browser) => Err(match browser {
                SourceError::Net(NetError::Blocked(detail)) => {
                    SourceError::Net(NetError::Blocked(format!("{detail}; the HTML endpoint: {}", error.message())))
                }
                other => SourceError::Parse(format!("the HTML endpoint: {}; the browser: {}", error.message(), other.message())),
            }),
        },
    }
}

/// Errors the browser would not fix (no network at all).
fn error_is_final(error: &SourceError) -> bool {
    matches!(error, SourceError::Net(net) if net.is_unreachable())
}

async fn via_http(engine: &Engine, q: &Query) -> Result<Found, SourceError> {
    let request = Request { keep_errors: true, ..Request::get(url(&engine.config().duckduckgo_html, q)) }
        .header("accept", "text/html")
        .header("accept-language", "en-US,en;q=0.8");
    let response = engine.net.get(request).await?;
    let html = response.text();
    if response.status == 202 || response.status == 403 || response.status == 429 {
        let why = match parse(&html) {
            Page::Blocked(why) => why,
            _ => format!("DuckDuckGo answered HTTP {}", response.status),
        };
        return Err(SourceError::Net(NetError::Blocked(why)));
    }
    if response.status >= 400 {
        return Err(SourceError::Net(crate::net::status_error(response.status, "html.duckduckgo.com", &response.headers, &response.body)));
    }
    match parse(&html) {
        Page::Results(hits) => Ok(Found { hits, via: "duckduckgo-html".into(), note: None }),
        Page::Blocked(why) => Err(SourceError::Net(NetError::Blocked(why))),
        Page::Unknown(title) => Err(SourceError::Parse(format!("DuckDuckGo returned a page without results (title: {title:?})"))),
    }
}

/// JavaScript run in the result page: result links, or what blocked them.
const EXTRACT: &str = r#"(() => {
  const text = document.body ? document.body.innerText.slice(0, 5000) : '';
  const blocked = /captcha|unusual traffic|are you a robot|bots use duckduckgo/i.test(text)
    || !!document.querySelector('.anomaly-modal, #challenge-form, iframe[src*="captcha"]');
  const results = [...document.querySelectorAll('.result:not(.result--ad), article[data-testid="result"]')].map((r) => {
    const a = r.querySelector('a.result__a, a[data-testid="result-title-a"]');
    const s = r.querySelector('.result__snippet, [data-result="snippet"]');
    return a ? { title: a.innerText.trim(), href: a.getAttribute('href') || a.href, snippet: s ? s.innerText.trim() : '' } : null;
  }).filter(Boolean);
  const none = !!document.querySelector('.no-results') || /no results\./i.test(text);
  return { blocked, none, results, title: document.title };
})()"#;

async fn via_browser(engine: &Engine, q: &Query) -> Result<Found, SourceError> {
    let peers = engine.peers.clone();
    let cancel = CancellationToken::new();
    let page = url(&engine.config().duckduckgo_html, q);
    let opened = peers.call("browser.open", json!({ "url": page, "background": true }), cancel.clone()).await.map_err(|error| {
        SourceError::NotConfigured(format!("the built-in browser is not available ({}): it runs with the CortexCode window", error.message))
    })?;
    let tab = opened.get("tab_id").and_then(Value::as_str).unwrap_or_default().to_owned();
    if let Some(load_error) = opened.get("load_error").and_then(Value::as_str) {
        let _ = peers.call("browser.close", json!({ "tab_id": tab }), cancel.clone()).await;
        return Err(SourceError::Net(NetError::Unreachable(format!("the browser could not load the search page: {load_error}"))));
    }
    let evaluated = peers.call("browser.eval", json!({ "tab_id": tab, "expression": EXTRACT }), cancel.clone()).await;
    let _ = peers.call("browser.close", json!({ "tab_id": tab }), cancel).await;
    let value = evaluated.map_err(|error| SourceError::Parse(format!("reading the result page in the browser: {}", error.message)))?;
    let value = value.get("value").cloned().unwrap_or(value);
    browser_results(&value)
}

/// Hits from the extraction script's answer.
pub fn browser_results(value: &Value) -> Result<Found, SourceError> {
    let results = value.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut hits = Vec::new();
    for result in &results {
        let text = |key: &str| result.get(key).and_then(Value::as_str).map(squash).unwrap_or_default();
        let Some(target) = unwrap_link(&text("href")) else { continue };
        let title = text("title");
        if title.is_empty() {
            continue;
        }
        hits.push(SearchHit { source: sources::WEB.into(), title, url: target, snippet: clip(&text("snippet"), 400), ..SearchHit::default() });
    }
    if hits.is_empty() && value.get("blocked").and_then(Value::as_bool) == Some(true) {
        return Err(SourceError::Net(NetError::Blocked("DuckDuckGo showed a bot check in the browser too (not solved)".into())));
    }
    if hits.is_empty() && value.get("none").and_then(Value::as_bool) != Some(true) {
        let title = value.get("title").and_then(Value::as_str).unwrap_or_default();
        return Err(SourceError::Parse(format!("the browser's result page had no results (title: {title:?})")));
    }
    Ok(Found { hits, via: "browser".into(), note: None })
}
