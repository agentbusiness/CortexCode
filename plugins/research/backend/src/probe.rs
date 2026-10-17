//! `plugin.probe`: exercise the real implementation without the network. Every source's parser on a
//! recorded answer (built into the binary), readability on an article, PDF extraction on a PDF made
//! here, and the deep job's chunking and source picking. `network` says whether arXiv's API host
//! answers a TCP connection, `github` whether `gh` is signed in, and `notes` whether the memory
//! plugin (which keeps the notes) answers; all three pass either way (research works offline for
//! notes and the cache and reports `offline` plainly otherwise; the memory plugin may start after
//! this one, and note calls fail plainly while it is away), so a machine without a network still
//! gets the plugin.

use crate::service::Core;
use crate::sources::{arxiv, github, hackernews, reddit, web};
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use std::time::{Duration, Instant};

fn check(name: &str, started: Instant, outcome: Result<String, String>) -> ProbeCheck {
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(detail) => ProbeCheck { name: name.into(), ok: true, detail, ms },
        Err(detail) => ProbeCheck { name: name.into(), ok: false, detail, ms },
    }
}

fn parsers() -> Result<String, String> {
    let arxiv = arxiv::parse(include_str!("../probe/arxiv_search.xml"))?;
    let hn = hackernews::parse(include_str!("../probe/hn_search.json"))?;
    let repos = github::parse(include_str!("../probe/github_repos.json"), github::Kind::Repos)?;
    let reddit = reddit::parse_rss(include_str!("../probe/reddit_search.rss"))?;
    let web::Page::Results(ddg) = web::parse(include_str!("../probe/ddg_search.html")) else {
        return Err("the DuckDuckGo result page did not parse".into());
    };
    let counts = [("arxiv", arxiv.len()), ("hackernews", hn.len()), ("github", repos.len()), ("reddit", reddit.len()), ("web", ddg.len())];
    if let Some((name, _)) = counts.iter().find(|(_, n)| *n == 0) {
        return Err(format!("{name}: no results parsed from its recorded answer"));
    }
    if arxiv.iter().any(|h| h.date.is_none() || !h.url.starts_with("https://arxiv.org/abs/")) {
        return Err("arxiv hits lack dates or abs URLs".into());
    }
    Ok(counts.iter().map(|(name, n)| format!("{name} {n}")).collect::<Vec<_>>().join(", "))
}

fn readability() -> Result<String, String> {
    let paragraph = "Speculative decoding drafts several tokens with a small model and verifies them with the large one in a single pass. ";
    let page = format!(
        "<html><head><title>Probe article</title><meta property=\"article:published_time\" content=\"2025-03-04T10:00:00Z\"></head><body><nav><a href=\"/\">Home</a> <a href=\"/about\">About</a></nav><article><h1>Probe article</h1><p>{}</p><p>{}</p></article><footer>Copyright footer text</footer></body></html>",
        paragraph.repeat(4),
        paragraph.repeat(3)
    );
    let got = crate::extract::html(&page, "https://example.com/probe");
    if !got.text.contains("Speculative decoding drafts several tokens") || got.text.contains("Copyright footer") || got.published.as_deref() != Some("2025-03-04") {
        return Err(format!("unexpected extraction: {:?}", crate::text::clip(&got.text, 200)));
    }
    Ok(format!("article text {} chars, date {}", got.text.len(), got.published.unwrap_or_default()))
}

/// A one-page PDF with a line of text in a standard font.
pub fn sample_pdf(text: &str) -> Vec<u8> {
    use pdf_extract::content::{Content, Operation};
    use pdf_extract::{Dictionary, Document, Object, Stream};
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut font = Dictionary::new();
    font.set("Type", Object::Name(b"Font".to_vec()));
    font.set("Subtype", Object::Name(b"Type1".to_vec()));
    font.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
    font.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
    let font_id = doc.add_object(font);
    let mut fonts = Dictionary::new();
    fonts.set("F1", Object::Reference(font_id));
    let mut resources = Dictionary::new();
    resources.set("Font", Object::Dictionary(fonts));
    let resources_id = doc.add_object(resources);
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), Object::Integer(12)]),
            Operation::new("Td", vec![Object::Integer(72), Object::Integer(720)]),
            Operation::new("Tj", vec![Object::string_literal(text)]),
            Operation::new("ET", vec![]),
        ],
    };
    let content_id = doc.add_object(Stream::new(Dictionary::new(), content.encode().unwrap_or_default()));
    let mut page = Dictionary::new();
    page.set("Type", Object::Name(b"Page".to_vec()));
    page.set("Parent", Object::Reference(pages_id));
    page.set("Contents", Object::Reference(content_id));
    let page_id = doc.add_object(page);
    let mut pages = Dictionary::new();
    pages.set("Type", Object::Name(b"Pages".to_vec()));
    pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
    pages.set("Count", Object::Integer(1));
    pages.set("Resources", Object::Reference(resources_id));
    pages.set("MediaBox", Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()]));
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let mut catalog = Dictionary::new();
    catalog.set("Type", Object::Name(b"Catalog".to_vec()));
    catalog.set("Pages", Object::Reference(pages_id));
    let catalog_id = doc.add_object(catalog);
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    let _ = doc.save_to(&mut bytes);
    bytes
}

fn pdf() -> Result<String, String> {
    let token = format!("probe {}", uuid::Uuid::new_v4().simple());
    let got = crate::extract::pdf(&sample_pdf(&format!("Research probe text {token}")))?;
    if !got.text.contains(&token) || got.pages != Some(1) {
        return Err(format!("extracted {:?}", got.text));
    }
    Ok("one-page PDF made and read back".into())
}

async fn notes(core: &Core) -> String {
    match core.notes.check().await {
        Ok(()) => "the memory plugin keeps the notes and answers".into(),
        Err(error) => format!("unavailable now ({}); note calls fail plainly until the memory plugin answers", error.message),
    }
}

fn deep() -> Result<String, String> {
    let text = (0..20).map(|i| format!("Paragraph {i}. {}", "word ".repeat(50))).collect::<Vec<_>>().join("\n\n");
    let chunks = crate::deep::chunk(&text, 1_000, 3);
    if chunks.len() != 3 || chunks.iter().any(|c| c.chars().count() > 1_000) {
        return Err(format!("chunking gave {} chunks", chunks.len()));
    }
    let schema = crate::deep::map_schema(3);
    if schema["properties"]["notes"]["items"]["required"] != serde_json::json!(["question", "claim", "quote"]) {
        return Err("map schema".into());
    }
    Ok("chunking and schemas".into())
}

async fn network(core: &Core) -> String {
    let Some(host) = url::Url::parse(&core.config.arxiv_api).ok().and_then(|u| Some((u.host_str()?.to_owned(), u.port_or_known_default()?))) else {
        return "problem: the arxiv_api endpoint is not a URL".into();
    };
    let started = Instant::now();
    match tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect((host.0.as_str(), host.1))).await {
        Ok(Ok(_)) => format!("online: {} answers ({} ms)", host.0, started.elapsed().as_millis()),
        Ok(Err(error)) => format!("offline: {} cannot be reached ({error}); search and fetch report offline, notes and the cache work", host.0),
        Err(_) => format!("offline: {} did not answer in 3 s; search and fetch report offline, notes and the cache work", host.0),
    }
}

pub async fn run(core: &Core) -> ProbeReport {
    let mut checks = Vec::new();
    let started = Instant::now();
    checks.push(check("parsers", started, parsers()));
    let started = Instant::now();
    checks.push(check("readability", started, readability()));
    let started = Instant::now();
    let outcome = tokio::task::spawn_blocking(pdf).await.unwrap_or_else(|error| Err(error.to_string()));
    checks.push(check("pdf", started, outcome));
    let started = Instant::now();
    let detail = notes(core).await;
    checks.push(ProbeCheck { name: "notes".into(), ok: true, detail, ms: started.elapsed().as_millis() as u64 });
    let started = Instant::now();
    checks.push(check("deep", started, deep()));
    let started = Instant::now();
    let detail = network(core).await;
    checks.push(ProbeCheck { name: "network".into(), ok: true, detail, ms: started.elapsed().as_millis() as u64 });
    let started = Instant::now();
    let detail = if core.engine.gh_ready().await { "gh is signed in: GitHub searches use it".to_owned() } else { "gh is not signed in (or not installed): GitHub searches use the public REST API".to_owned() };
    checks.push(ProbeCheck { name: "github".into(), ok: true, detail, ms: started.elapsed().as_millis() as u64 });
    ProbeReport::from_checks(checks)
}
