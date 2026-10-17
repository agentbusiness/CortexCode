//! Hacker News through the Algolia HN Search API (`hn.algolia.com/api/v1`, key-free): stories
//! matching the query, by relevance and points (`/search`) or newest first (`/search_by_date`);
//! `since` is `numericFilters=created_at_i>=<unix seconds>`.

use super::{Engine, Found, Query, SourceError, Sort, query_string};
use crate::net::Request;
use crate::text::{clip, normalize_date, squash, strip_tags};
use cc_protocol::contracts::research::{SearchHit, sources};
use serde_json::{Value, json};

pub fn url(base: &str, q: &Query) -> String {
    let path = if q.sort == Sort::Recent { "search_by_date" } else { "search" };
    let mut pairs = vec![("query", q.text.trim().to_owned()), ("tags", "story".to_owned()), ("hitsPerPage", q.limit.to_string())];
    if let Some(since) = q.since.as_deref().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()) {
        let seconds = since.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp()).unwrap_or(0);
        pairs.push(("numericFilters", format!("created_at_i>={seconds}")));
    }
    format!("{}/{path}?{}", base.trim_end_matches('/'), query_string(&pairs))
}

pub async fn search(engine: &Engine, q: &Query) -> Result<Found, SourceError> {
    let response = engine.net.get(Request::get(url(&engine.config().hackernews_api, q))).await?;
    let mut hits = parse(&response.text()).map_err(SourceError::Parse)?;
    if q.sort == Sort::Popular {
        hits.sort_by(|a, b| b.score.unwrap_or(0.0).total_cmp(&a.score.unwrap_or(0.0)));
    }
    Ok(Found { hits, via: "api".into(), note: None })
}

pub fn parse(body: &str) -> Result<Vec<SearchHit>, String> {
    let value: Value = serde_json::from_str(body).map_err(|error| format!("not JSON: {error}"))?;
    let items = value.get("hits").and_then(Value::as_array).ok_or_else(|| {
        format!("no `hits` in the answer{}", value.get("message").and_then(Value::as_str).map(|m| format!(": {m}")).unwrap_or_default())
    })?;
    let mut hits = Vec::new();
    for item in items {
        let s = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
        let id = s("objectID");
        let title = squash(&s("title"));
        if id.is_empty() || title.is_empty() {
            continue;
        }
        let discussion = format!("https://news.ycombinator.com/item?id={id}");
        let link = s("url");
        let mut extra = serde_json::Map::new();
        extra.insert("discussion".into(), json!(discussion));
        if let Some(comments) = item.get("num_comments").and_then(Value::as_u64) {
            extra.insert("comments".into(), json!(comments));
        }
        let author = s("author");
        hits.push(SearchHit {
            source: sources::HACKERNEWS.into(),
            title,
            url: if link.is_empty() { discussion } else { link },
            date: normalize_date(&s("created_at")),
            authors: if author.is_empty() { Vec::new() } else { vec![author] },
            snippet: clip(&strip_tags(&s("story_text")), 400),
            score: item.get("points").and_then(Value::as_f64),
            extra,
        });
    }
    Ok(hits)
}
