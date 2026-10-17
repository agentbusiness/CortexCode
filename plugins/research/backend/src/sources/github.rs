//! GitHub search: repositories (`github`), issues and pull requests (`github_issues`) and code
//! (`github_code`). Through `gh api` when the GitHub CLI is installed and signed in (the user's own
//! sign-in and its higher limits), else the public REST API (about 10 searches a minute without a
//! token; a token the user exported in `GH_TOKEN` / `GITHUB_TOKEN` is used when present). Both
//! paths return the same REST JSON, parsed by the same functions. Code search requires sign-in.
//! `recent` sorts by last update, `popular` by stars (repositories) or comments (issues); `since`
//! becomes `pushed:>=` (repositories) or `updated:>=` (issues).

use super::{Engine, Found, Query, SourceError, Sort, query_string};
use crate::net::{NetError, Request};
use crate::text::{clip, normalize_date, squash};
use cc_protocol::contracts::research::{SearchHit, sources};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Repos,
    Issues,
    Code,
}

impl Kind {
    fn path(self) -> &'static str {
        match self {
            Kind::Repos => "search/repositories",
            Kind::Issues => "search/issues",
            Kind::Code => "search/code",
        }
    }
}

/// The REST query parameters for a search.
pub fn params(q: &Query, kind: Kind) -> Vec<(&'static str, String)> {
    let mut text = q.text.trim().to_owned();
    if let Some(since) = &q.since {
        match kind {
            Kind::Repos => text.push_str(&format!(" pushed:>={since}")),
            Kind::Issues => text.push_str(&format!(" updated:>={since}")),
            Kind::Code => {}
        }
    }
    let mut pairs = vec![("q", text)];
    let sort = match (kind, q.sort) {
        (Kind::Repos, Sort::Recent) | (Kind::Issues, Sort::Recent) => Some("updated"),
        (Kind::Repos, Sort::Popular) => Some("stars"),
        (Kind::Issues, Sort::Popular) => Some("comments"),
        _ => None,
    };
    if let Some(sort) = sort {
        pairs.push(("sort", sort.to_owned()));
        pairs.push(("order", "desc".to_owned()));
    }
    pairs.push(("per_page", q.limit.to_string()));
    pairs
}

/// `gh auth status` exits 0.
pub async fn gh_signed_in(gh: &str) -> bool {
    let mut command = tokio::process::Command::new(gh);
    command.args(["auth", "status"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(10), command.status()).await {
        Ok(Ok(status)) => status.success(),
        _ => false,
    }
}

pub async fn search(engine: &Engine, q: &Query, kind: Kind) -> Result<Found, SourceError> {
    let config = engine.config();
    let use_gh = match config.github.as_str() {
        "rest" => false,
        "gh" => true,
        _ => engine.gh_ready().await,
    };
    let (body, via) = if use_gh {
        (via_gh(&config.gh, q, kind).await?, "gh")
    } else {
        let token = ["GH_TOKEN", "GITHUB_TOKEN"].iter().find_map(|name| (engine.env)(name).filter(|v| !v.trim().is_empty()));
        if kind == Kind::Code && token.is_none() {
            return Err(SourceError::NotConfigured(
                "GitHub code search needs sign-in: run `gh auth login` (or export GH_TOKEN)".into(),
            ));
        }
        let url = format!("{}/{}?{}", config.github_api.trim_end_matches('/'), kind.path(), query_string(&params(q, kind)));
        let mut request = Request::get(url)
            .header("accept", if kind == Kind::Code { "application/vnd.github.text-match+json" } else { "application/vnd.github+json" })
            .header("x-github-api-version", "2022-11-28");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {}", token.trim()));
        }
        (engine.net.get(request).await?.text(), "rest")
    };
    let hits = parse(&body, kind).map_err(SourceError::Parse)?;
    Ok(Found { hits, via: via.into(), note: None })
}

async fn via_gh(gh: &str, q: &Query, kind: Kind) -> Result<String, SourceError> {
    let mut command = tokio::process::Command::new(gh);
    command.args(["api", "-X", "GET", kind.path()]);
    if kind == Kind::Code {
        command.args(["-H", "Accept: application/vnd.github.text-match+json"]);
    }
    for (key, value) in params(q, kind) {
        command.arg("-f").arg(format!("{key}={value}"));
    }
    command.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = match tokio::time::timeout(Duration::from_secs(30), command.output()).await {
        Err(_) => return Err(SourceError::Net(NetError::Timeout("gh api did not answer in 30 s".into()))),
        Ok(Err(error)) => return Err(SourceError::NotConfigured(format!("cannot run {gh}: {error}"))),
        Ok(Ok(output)) => output,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = squash(&String::from_utf8_lossy(&output.stderr));
    let detail = if stderr.is_empty() { clip(&stdout, 200) } else { clip(&stderr, 300) };
    let lower = detail.to_lowercase();
    Err(if lower.contains("rate limit") {
        SourceError::Net(NetError::RateLimited(format!("GitHub (gh): {detail}")))
    } else if lower.contains("could not resolve") || lower.contains("dial tcp") || lower.contains("no such host") || lower.contains("network is unreachable") {
        SourceError::Net(NetError::Unreachable(format!("api.github.com (gh): {detail}")))
    } else {
        SourceError::Parse(format!("gh api: {detail}"))
    })
}

/// Parse a REST search answer of the given kind.
pub fn parse(body: &str, kind: Kind) -> Result<Vec<SearchHit>, String> {
    let value: Value = serde_json::from_str(body).map_err(|error| format!("not JSON: {error}"))?;
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        let message = value.get("message").and_then(Value::as_str).unwrap_or("no `items` in the answer");
        return Err(format!("GitHub: {message}"));
    };
    let s = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).map(squash).unwrap_or_default();
    let mut hits = Vec::new();
    for item in items {
        let mut extra = serde_json::Map::new();
        let hit = match kind {
            Kind::Repos => {
                let stars = item.get("stargazers_count").and_then(Value::as_f64);
                let language = s(item, "language");
                let topics: Vec<String> = item.get("topics").and_then(Value::as_array).map(|t| t.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
                if let Some(stars) = stars {
                    extra.insert("stars".into(), json!(stars as u64));
                }
                if !language.is_empty() {
                    extra.insert("language".into(), json!(language));
                }
                if !topics.is_empty() {
                    extra.insert("topics".into(), json!(topics));
                }
                for key in ["pushed_at", "created_at"] {
                    if let Some(date) = item.get(key).and_then(Value::as_str).and_then(normalize_date) {
                        extra.insert(key.trim_end_matches("_at").into(), json!(date));
                    }
                }
                if item.get("archived").and_then(Value::as_bool) == Some(true) {
                    extra.insert("archived".into(), json!(true));
                }
                let owner = item.get("owner").and_then(|o| o.get("login")).and_then(Value::as_str).map(str::to_owned);
                SearchHit {
                    source: sources::GITHUB.into(),
                    title: s(item, "full_name"),
                    url: s(item, "html_url"),
                    date: item.get("updated_at").and_then(Value::as_str).and_then(normalize_date),
                    authors: owner.into_iter().collect(),
                    snippet: clip(&s(item, "description"), 400),
                    score: stars,
                    extra,
                }
            }
            Kind::Issues => {
                let repo = s(item, "repository_url").rsplit("/repos/").next().unwrap_or_default().to_owned();
                extra.insert("repo".into(), json!(repo));
                extra.insert("state".into(), json!(s(item, "state")));
                extra.insert("pull_request".into(), json!(item.get("pull_request").is_some()));
                if let Some(comments) = item.get("comments").and_then(Value::as_u64) {
                    extra.insert("comments".into(), json!(comments));
                }
                if let Some(created) = item.get("created_at").and_then(Value::as_str).and_then(normalize_date) {
                    extra.insert("created".into(), json!(created));
                }
                let author = item.get("user").and_then(|u| u.get("login")).and_then(Value::as_str).map(str::to_owned);
                SearchHit {
                    source: sources::GITHUB_ISSUES.into(),
                    title: format!("{repo}: {}", s(item, "title")),
                    url: s(item, "html_url"),
                    date: item.get("updated_at").and_then(Value::as_str).and_then(normalize_date),
                    authors: author.into_iter().collect(),
                    snippet: clip(&s(item, "body"), 400),
                    score: item.get("reactions").and_then(|r| r.get("total_count")).and_then(Value::as_f64),
                    extra,
                }
            }
            Kind::Code => {
                let repo = item.get("repository").and_then(|r| r.get("full_name")).and_then(Value::as_str).unwrap_or_default().to_owned();
                let path = s(item, "path");
                let fragments: Vec<String> = item
                    .get("text_matches")
                    .and_then(Value::as_array)
                    .map(|m| m.iter().filter_map(|m| m.get("fragment").and_then(Value::as_str)).map(squash).collect())
                    .unwrap_or_default();
                extra.insert("repo".into(), json!(repo));
                extra.insert("path".into(), json!(path));
                SearchHit {
                    source: sources::GITHUB_CODE.into(),
                    title: format!("{repo}: {path}"),
                    url: s(item, "html_url"),
                    date: None,
                    authors: Vec::new(),
                    snippet: clip(&fragments.join(" … "), 400),
                    score: None,
                    extra,
                }
            }
        };
        if !hit.url.is_empty() {
            hits.push(hit);
        }
    }
    Ok(hits)
}
