//! arXiv: the export API (Atom). `search_query` ANDs the words (`all:`), keeps quoted phrases, and
//! limits by `submittedDate` for `since`; `sortBy` is `relevance` or `submittedDate` (recent).
//! Requests are spaced 3 s apart (arXiv's API terms).

use super::{Engine, Found, Query, SourceError, Sort, query_string};
use crate::net::Request;
use crate::text::{clip, normalize_date, squash};
use cc_protocol::contracts::research::{SearchHit, sources};
use serde_json::json;

const ATOM: &str = "http://www.w3.org/2005/Atom";
const ARXIV: &str = "http://arxiv.org/schemas/atom";

/// The `search_query` value: words ANDed over all fields, quoted phrases kept, a date range.
pub fn search_query(text: &str, since: Option<&str>) -> String {
    let mut terms = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix('"') {
            let end = stripped.find('"').unwrap_or(stripped.len());
            let phrase = squash(&stripped[..end]);
            if !phrase.is_empty() {
                terms.push(format!("all:\"{phrase}\""));
            }
            rest = stripped.get(end + 1..).unwrap_or("").trim_start();
        } else {
            let end = rest.find(|c: char| c.is_whitespace() || c == '"').unwrap_or(rest.len());
            let word: String = rest[..end].chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '_' | '+')).collect();
            if !word.is_empty() {
                terms.push(format!("all:{word}"));
            }
            rest = rest[end..].trim_start();
        }
    }
    if let Some(since) = since {
        terms.push(format!("submittedDate:[{}0000 TO 299912312359]", since.replace('-', "")));
    }
    terms.join(" AND ")
}

pub fn url(base: &str, q: &Query) -> String {
    let sort = match q.sort {
        Sort::Recent => "submittedDate",
        _ => "relevance",
    };
    let pairs = [
        ("search_query", search_query(&q.text, q.since.as_deref())),
        ("start", "0".to_owned()),
        ("max_results", q.limit.to_string()),
        ("sortBy", sort.to_owned()),
        ("sortOrder", "descending".to_owned()),
    ];
    format!("{base}?{}", query_string(&pairs))
}

pub async fn search(engine: &Engine, q: &Query) -> Result<Found, SourceError> {
    let response = engine.net.get(Request::get(url(&engine.config().arxiv_api, q))).await?;
    let hits = parse(&response.text()).map_err(SourceError::Parse)?;
    Ok(Found { hits, via: "atom".into(), note: None })
}

/// Parse an arXiv API Atom feed.
pub fn parse(xml: &str) -> Result<Vec<SearchHit>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|error| format!("not an Atom feed: {error}"))?;
    let feed = doc.root_element();
    if !feed.has_tag_name((ATOM, "feed")) {
        return Err(format!("expected an Atom feed, got <{}>", feed.tag_name().name()));
    }
    let child_text = |node: roxmltree::Node, ns: &str, name: &str| {
        node.children().find(|c| c.has_tag_name((ns, name))).and_then(|c| c.text()).map(squash).unwrap_or_default()
    };
    let mut hits = Vec::new();
    for entry in feed.children().filter(|c| c.has_tag_name((ATOM, "entry"))) {
        let id = child_text(entry, ATOM, "id");
        let title = child_text(entry, ATOM, "title");
        if id.contains("/api/errors") || title == "Error" {
            return Err(format!("arXiv: {}", child_text(entry, ATOM, "summary")));
        }
        if id.is_empty() {
            continue;
        }
        let url = id.replacen("http://", "https://", 1);
        let arxiv_id = url.rsplit("/abs/").next().unwrap_or_default().to_owned();
        let authors: Vec<String> = entry
            .children()
            .filter(|c| c.has_tag_name((ATOM, "author")))
            .map(|author| child_text(author, ATOM, "name"))
            .filter(|name| !name.is_empty())
            .collect();
        let pdf = entry
            .children()
            .find(|c| c.has_tag_name((ATOM, "link")) && (c.attribute("title") == Some("pdf") || c.attribute("type") == Some("application/pdf")))
            .and_then(|link| link.attribute("href"))
            .map(|href| href.replacen("http://", "https://", 1));
        let category = entry.children().find(|c| c.has_tag_name((ARXIV, "primary_category"))).and_then(|c| c.attribute("term"));
        let mut extra = serde_json::Map::new();
        extra.insert("arxiv_id".into(), json!(arxiv_id));
        if let Some(pdf) = pdf {
            extra.insert("pdf".into(), json!(pdf));
        }
        if let Some(category) = category {
            extra.insert("category".into(), json!(category));
        }
        if let Some(updated) = normalize_date(&child_text(entry, ATOM, "updated")) {
            extra.insert("updated".into(), json!(updated));
        }
        hits.push(SearchHit {
            source: sources::ARXIV.into(),
            title,
            url,
            date: normalize_date(&child_text(entry, ATOM, "published")),
            authors,
            snippet: clip(&child_text(entry, ATOM, "summary"), 500),
            score: None,
            extra,
        });
    }
    Ok(hits)
}
