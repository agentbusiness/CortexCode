//! Semantic Scholar: the Academic Graph API's free tier. Relevance order uses `/paper/search`;
//! `recent` and `popular` use `/paper/search/bulk` sorted by `publicationDate:desc` or
//! `citationCount:desc` (only the bulk endpoint sorts). `since` is `publicationDateOrYear=<date>:`.
//! Without a key the requests share a public pool that is often rate limited (429): the request is
//! retried when the server asks for a short wait, then reported as rate limited. A key the user set
//! in `SEMANTIC_SCHOLAR_API_KEY` (or `S2_API_KEY`) is sent as `x-api-key`; none is required.

use super::{Engine, Found, Query, SourceError, Sort, query_string};
use crate::net::Request;
use crate::text::{clip, normalize_date, squash};
use cc_protocol::contracts::research::{SearchHit, sources};
use serde_json::{Value, json};

pub const FIELDS: &str = "title,url,abstract,authors,year,publicationDate,citationCount,externalIds,openAccessPdf,venue";

pub fn url(base: &str, q: &Query) -> String {
    let mut pairs = vec![("query", q.text.clone()), ("fields", FIELDS.to_owned())];
    if let Some(since) = &q.since {
        pairs.push(("publicationDateOrYear", format!("{since}:")));
    }
    let path = match q.sort {
        Sort::Relevance => {
            pairs.push(("limit", q.limit.to_string()));
            "paper/search"
        }
        Sort::Recent => {
            pairs.push(("sort", "publicationDate:desc".to_owned()));
            "paper/search/bulk"
        }
        Sort::Popular => {
            pairs.push(("sort", "citationCount:desc".to_owned()));
            "paper/search/bulk"
        }
    };
    format!("{}/{path}?{}", base.trim_end_matches('/'), query_string(&pairs))
}

pub async fn search(engine: &Engine, q: &Query) -> Result<Found, SourceError> {
    let mut request = Request::get(url(&engine.config().semantic_scholar_api, q));
    let key = ["SEMANTIC_SCHOLAR_API_KEY", "S2_API_KEY"].iter().find_map(|name| (engine.env)(name).filter(|v| !v.trim().is_empty()));
    if let Some(key) = key {
        request = request.header("x-api-key", key.trim());
    }
    let response = engine.net.get(request).await.map_err(|error| match error {
        crate::net::NetError::RateLimited(detail) => crate::net::NetError::RateLimited(format!(
            "{detail}; the unauthenticated pool is shared by everyone (a key in SEMANTIC_SCHOLAR_API_KEY gives a personal limit)"
        )),
        other => other,
    })?;
    let mut hits = parse(&response.text()).map_err(SourceError::Parse)?;
    hits.truncate(q.limit as usize);
    Ok(Found { hits, via: "api".into(), note: None })
}

/// Parse a `/paper/search` or `/paper/search/bulk` answer.
pub fn parse(body: &str) -> Result<Vec<SearchHit>, String> {
    let value: Value = serde_json::from_str(body).map_err(|error| format!("not JSON: {error}"))?;
    if let Some(message) = value.get("message").or_else(|| value.get("error")).and_then(Value::as_str)
        && value.get("data").is_none()
    {
        return Err(format!("Semantic Scholar: {message}"));
    }
    let Some(data) = value.get("data").and_then(Value::as_array) else {
        // A search with no match has `total: 0` and no `data`.
        return if value.get("total").is_some() { Ok(Vec::new()) } else { Err("no `data` in the answer".into()) };
    };
    let mut hits = Vec::new();
    for paper in data {
        let text = |key: &str| paper.get(key).and_then(Value::as_str).map(squash).unwrap_or_default();
        let title = text("title");
        let id = text("paperId");
        if title.is_empty() {
            continue;
        }
        let url = Some(text("url")).filter(|u| !u.is_empty()).unwrap_or_else(|| format!("https://www.semanticscholar.org/paper/{id}"));
        let authors = paper
            .get("authors")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(|a| a.get("name").and_then(Value::as_str)).map(str::to_owned).collect())
            .unwrap_or_default();
        let date = paper
            .get("publicationDate")
            .and_then(Value::as_str)
            .and_then(normalize_date)
            .or_else(|| paper.get("year").and_then(Value::as_u64).map(|year| format!("{year}-01-01")));
        let mut extra = serde_json::Map::new();
        extra.insert("paper_id".into(), json!(id));
        if let Some(ids) = paper.get("externalIds").and_then(Value::as_object) {
            if let Some(arxiv) = ids.get("ArXiv").and_then(Value::as_str) {
                extra.insert("arxiv_id".into(), json!(arxiv));
            }
            if let Some(doi) = ids.get("DOI").and_then(Value::as_str) {
                extra.insert("doi".into(), json!(doi));
            }
        }
        if let Some(pdf) = paper.get("openAccessPdf").and_then(|p| p.get("url")).and_then(Value::as_str).filter(|u| !u.is_empty()) {
            extra.insert("pdf".into(), json!(pdf));
        }
        let venue = text("venue");
        if !venue.is_empty() {
            extra.insert("venue".into(), json!(venue));
        }
        let citations = paper.get("citationCount").and_then(Value::as_f64);
        if let Some(count) = citations {
            extra.insert("citations".into(), json!(count as u64));
        }
        hits.push(SearchHit {
            source: sources::SEMANTIC_SCHOLAR.into(),
            title,
            url,
            date,
            authors,
            snippet: clip(&text("abstract"), 500),
            score: citations,
            extra,
        });
    }
    Ok(hits)
}
