//! Reddit search without an account: `/search.json` (the Listing JSON) with a descriptive
//! User-Agent, and when Reddit refuses it (it often answers unauthenticated JSON with 403 "blocked
//! by network security") the same search as `/search.rss` (Atom), which it still serves. Posts only
//! (`type=link`); `recent` is `sort=new`, `popular` is `sort=top`; `since` picks the smallest period
//! (`t=day|week|month|year|all`) that covers it and the results are filtered by date as well.

use super::{Engine, Found, Query, SourceError, Sort, days_since, query_string};
use crate::net::{NetError, Request};
use crate::text::{clip, decode_entities, normalize_date, squash, strip_tags};
use cc_protocol::contracts::research::{SearchHit, sources};
use serde_json::{Value, json};

const ATOM: &str = "http://www.w3.org/2005/Atom";

pub fn params(q: &Query) -> Vec<(&'static str, String)> {
    let sort = match q.sort {
        Sort::Relevance => "relevance",
        Sort::Recent => "new",
        Sort::Popular => "top",
    };
    let period = match q.since.as_deref().and_then(days_since) {
        Some(days) if days <= 1 => "day",
        Some(days) if days <= 7 => "week",
        Some(days) if days <= 31 => "month",
        Some(days) if days <= 365 => "year",
        _ => "all",
    };
    vec![
        ("q", q.text.trim().to_owned()),
        ("sort", sort.to_owned()),
        ("t", period.to_owned()),
        ("type", "link".to_owned()),
        ("limit", q.limit.to_string()),
        ("raw_json", "1".to_owned()),
    ]
}

pub async fn search(engine: &Engine, q: &Query) -> Result<Found, SourceError> {
    let base = engine.config().reddit.trim_end_matches('/').to_owned();
    let query = query_string(&params(q));
    let json_error = match engine.net.get(Request::get(format!("{base}/search.json?{query}"))).await {
        Ok(response) => match parse_json(&response.text()) {
            Ok(hits) => return Ok(Found { hits, via: "json".into(), note: None }),
            Err(error) => SourceError::Parse(error),
        },
        Err(error) if error.is_unreachable() => return Err(error.into()),
        Err(error) => SourceError::Net(error),
    };
    // JSON refused or not JSON (a block page): the Atom feed of the same search.
    match engine.net.get(Request::get(format!("{base}/search.rss?{query}"))).await {
        Ok(response) => {
            let hits = parse_rss(&response.text()).map_err(SourceError::Parse)?;
            Ok(Found { hits, via: "rss".into(), note: Some(format!("search.json refused ({}); used search.rss", json_error.message())) })
        }
        Err(NetError::RateLimited(detail)) => Err(SourceError::Net(NetError::RateLimited(format!("{detail} (after search.json: {})", json_error.message())))),
        Err(error) => Err(SourceError::Net(error)),
    }
}

/// Parse a Listing (`/search.json`).
pub fn parse_json(body: &str) -> Result<Vec<SearchHit>, String> {
    let value: Value = serde_json::from_str(body).map_err(|_| format!("not JSON ({})", clip(&strip_tags(body), 120)))?;
    let children = value.get("data").and_then(|d| d.get("children")).and_then(Value::as_array).ok_or("no data.children in the answer")?;
    let mut hits = Vec::new();
    for child in children {
        if child.get("kind").and_then(Value::as_str) != Some("t3") {
            continue;
        }
        let Some(post) = child.get("data") else { continue };
        let s = |key: &str| post.get(key).and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
        let permalink = s("permalink");
        if permalink.is_empty() {
            continue;
        }
        let discussion = format!("https://www.reddit.com{permalink}");
        let mut extra = serde_json::Map::new();
        extra.insert("subreddit".into(), json!(s("subreddit")));
        extra.insert("discussion".into(), json!(discussion));
        if let Some(comments) = post.get("num_comments").and_then(Value::as_u64) {
            extra.insert("comments".into(), json!(comments));
        }
        let link = s("url");
        if post.get("is_self").and_then(Value::as_bool) == Some(false) && !link.is_empty() {
            extra.insert("link".into(), json!(link));
        }
        let author = s("author");
        hits.push(SearchHit {
            source: sources::REDDIT.into(),
            title: squash(&decode_entities(&s("title"))),
            url: discussion,
            date: post.get("created_utc").and_then(Value::as_f64).and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0)).map(|t| t.date_naive().to_string()),
            authors: if author.is_empty() { Vec::new() } else { vec![author] },
            snippet: clip(&squash(&s("selftext")), 400),
            score: post.get("score").and_then(Value::as_f64),
            extra,
        });
    }
    Ok(hits)
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|c| c.has_tag_name((ATOM, name)))
}

/// Parse the Atom feed of `/search.rss` (posts are entries with a `t3_` id; subreddits are `t5_`).
pub fn parse_rss(xml: &str) -> Result<Vec<SearchHit>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| format!("not a feed ({})", clip(&strip_tags(xml), 120)))?;
    let feed = doc.root_element();
    if !feed.has_tag_name((ATOM, "feed")) {
        return Err(format!("expected an Atom feed, got <{}>", feed.tag_name().name()));
    }
    let mut hits = Vec::new();
    for entry in feed.children().filter(|c| c.has_tag_name((ATOM, "entry"))) {
        let id = child(entry, "id").and_then(|n| n.text()).unwrap_or_default();
        if !id.starts_with("t3_") {
            continue;
        }
        let Some(url) = child(entry, "link").and_then(|n| n.attribute("href")).map(str::to_owned) else { continue };
        let author = child(entry, "author").and_then(|a| child(a, "name")).and_then(|n| n.text()).map(|n| n.trim_start_matches("/u/").to_owned());
        let subreddit = child(entry, "category").and_then(|c| c.attribute("term")).unwrap_or_default();
        let content = child(entry, "content").and_then(|n| n.text()).unwrap_or_default();
        // The content is HTML: the post's text (a "submitted by" footer and links follow it).
        let text = strip_tags(content);
        let text = text.split(" submitted by ").next().unwrap_or(&text).to_owned();
        let mut extra = serde_json::Map::new();
        extra.insert("subreddit".into(), json!(subreddit));
        extra.insert("discussion".into(), json!(url));
        let date = child(entry, "published").or_else(|| child(entry, "updated")).and_then(|n| n.text()).and_then(normalize_date);
        hits.push(SearchHit {
            source: sources::REDDIT.into(),
            title: squash(child(entry, "title").and_then(|n| n.text()).unwrap_or_default()),
            url,
            date,
            authors: author.into_iter().collect(),
            snippet: clip(&text, 400),
            score: None,
            extra,
        });
    }
    Ok(hits)
}
