//! Readable text from what `research.fetch` downloads.
//!
//! - **HTML**: `dom_smoothie`, a maintained Rust port of Mozilla's Readability.js (the article body
//!   without navigation, ads and footers), in its formatted text mode (paragraphs, list items and
//!   code lines kept apart). Dates come from the article metadata or the usual meta tags (Open
//!   Graph `article:published_time`, Highwire `citation_*` as on arXiv and journal pages, Dublin
//!   Core, schema.org, `<time datetime>`); authors from `citation_author` or the byline. A page that
//!   is only a `<meta http-equiv="refresh">` redirect is reported as such so the fetcher follows it.
//!   A page Readability cannot score falls back to the body text without scripts, styles and page
//!   chrome.
//! - **PDF**: `pdf-extract` (on `lopdf`), page by page. It emits text in content-stream order,
//!   which for LaTeX output (arXiv) is reading order, one column after the other; a PDF whose stream
//!   order differs from its visual order (some word processors) can come out interleaved, a known
//!   limit. The text is cleaned: ligatures expanded, words hyphenated at line ends joined, lines of a
//!   paragraph joined, bare page numbers dropped. The title is the document's Title entry when it is
//!   meaningful, else the first substantial line; the date is the creation date. The extractor runs
//!   under `catch_unwind` (it panics on some malformed files) and its error is reported.
//! - Anything textual (plain text, Markdown, JSON, XML, CSV) is returned as it is.

use crate::text::{normalize_date, squash};
use dom_smoothie::{Config as ReadabilityConfig, Readability, TextMode};

/// Bumped when extraction changes, so cached documents are extracted again from their stored bytes.
pub const EXTRACTOR_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extracted {
    pub title: String,
    pub text: String,
    pub published: Option<String>,
    pub authors: Vec<String>,
    pub site_name: Option<String>,
    pub pages: Option<u32>,
    /// A `<meta http-equiv="refresh">` page: where it sends the reader.
    pub redirect: Option<String>,
}

/// The kind of a response: `pdf` by type or magic bytes, `html` by type or a leading tag, `other`
/// for text, None for binary content that is neither.
pub fn kind_of(content_type: &str, body: &[u8]) -> Option<&'static str> {
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("application/pdf") || body.starts_with(b"%PDF-") {
        return Some("pdf");
    }
    let head = String::from_utf8_lossy(&body[..body.len().min(1_024)]).trim_start().to_ascii_lowercase();
    if ct.contains("html") || head.starts_with("<!doctype html") || head.starts_with("<html") {
        return Some("html");
    }
    let textual = ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") || ct.contains("markdown") || ct.contains("javascript") || ct.contains("csv");
    if textual || (ct.is_empty() && std::str::from_utf8(&body[..body.len().min(4_096)]).is_ok() && !body.contains(&0)) {
        return Some("other");
    }
    None
}

const DATE_METAS: &[(&str, &str)] = &[
    ("property", "article:published_time"),
    ("name", "citation_publication_date"),
    ("name", "citation_date"),
    ("name", "citation_online_date"),
    ("name", "dc.date"),
    ("name", "DC.date"),
    ("name", "dcterms.date"),
    ("name", "DC.date.issued"),
    ("itemprop", "datePublished"),
    ("name", "date"),
    ("name", "pubdate"),
    ("property", "og:published_time"),
];

/// Extract an HTML page. `url` resolves relative redirects and links.
pub fn html(page: &str, url: &str) -> Extracted {
    let doc = dom_query::Document::from(page);
    let meta = |attr: &str, name: &str| {
        doc.select(&format!("meta[{attr}=\"{name}\"]")).attr("content").map(|c| squash(&c)).filter(|c| !c.is_empty())
    };
    // A redirect page: a refresh meta and almost no text of its own.
    if let Some(refresh) = doc.select("meta[http-equiv=\"refresh\" i]").attr("content") {
        let body_text = squash(&doc.select("body").text());
        if let Some(at) = refresh.to_ascii_lowercase().find("url=")
            && body_text.chars().count() < 400
        {
            let target = refresh[at + 4..].trim().trim_matches(|c| c == '\'' || c == '"').to_owned();
            if let Some(absolute) = url::Url::parse(url).ok().and_then(|base| base.join(&target).ok()) {
                return Extracted { title: squash(&doc.select("title").text()), redirect: Some(absolute.to_string()), ..Extracted::default() };
            }
        }
    }
    let mut published = DATE_METAS.iter().find_map(|(attr, name)| meta(attr, name).and_then(|value| normalize_date(&value)));
    if published.is_none() {
        published = doc.select("time[datetime]").iter().find_map(|time| time.attr("datetime").and_then(|d| normalize_date(&d)));
    }
    let mut authors: Vec<String> = doc
        .select("meta[name=\"citation_author\"]")
        .iter()
        .filter_map(|m| m.attr("content").map(|c| squash(&c)))
        .filter(|a| !a.is_empty())
        .collect();
    if authors.is_empty()
        && let Some(author) = meta("name", "author")
    {
        authors.push(author);
    }
    let citation_title = meta("name", "citation_title");
    let site_name = meta("property", "og:site_name");
    let fallback_title = squash(&doc.select("title").text());
    let config = ReadabilityConfig { text_mode: TextMode::Formatted, ..ReadabilityConfig::default() };
    let article = Readability::with_document(doc.clone(), url::Url::parse(url).ok().map(|_| url), Some(config)).ok().and_then(|mut r| r.parse().ok());
    match article {
        Some(article) if article.text_content.trim().chars().count() >= 80 => {
            if published.is_none() {
                published = article.published_time.as_deref().and_then(normalize_date);
            }
            if authors.is_empty()
                && let Some(byline) = article.byline.as_deref().map(squash).filter(|b| !b.is_empty() && b.len() < 120 && normalize_date(b).is_none())
            {
                authors.push(byline);
            }
            Extracted {
                title: citation_title.unwrap_or_else(|| squash(&article.title)).chars().take(300).collect(),
                text: tidy(&article.text_content),
                published,
                authors,
                site_name: site_name.or(article.site_name),
                pages: None,
                redirect: None,
            }
        }
        _ => {
            for chrome in ["script", "style", "noscript", "nav", "header", "footer", "aside", "form", "svg", "iframe"] {
                doc.select(chrome).remove();
            }
            Extracted {
                title: citation_title.unwrap_or(fallback_title),
                text: tidy(&block_text(&doc)),
                published,
                authors,
                site_name,
                pages: None,
                redirect: None,
            }
        }
    }
}

/// The body's text with block elements on their own lines.
fn block_text(doc: &dom_query::Document) -> String {
    let mut out = String::new();
    for block in doc.select("body p, body li, body h1, body h2, body h3, body h4, body pre, body td, body blockquote").iter() {
        let text = squash(&block.text());
        if !text.is_empty() {
            out.push_str(&text);
            out.push_str("\n\n");
        }
    }
    if out.trim().is_empty() { doc.select("body").text().to_string() } else { out }
}

/// Trim every line, collapse runs of blank lines into one.
pub fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        let line = if line.trim().is_empty() { "" } else { line.trim_start_matches(|c: char| c.is_whitespace() && c != '\t') };
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(line);
    }
    out
}

/// Pages, Title, CreationDate and Author of a PDF.
type PdfParts = (Vec<String>, Option<String>, Option<String>, Vec<String>);

/// Extract a PDF's text, title, date and page count.
pub fn pdf(bytes: &[u8]) -> Result<Extracted, String> {
    let owned = bytes.to_vec();
    let outcome = std::panic::catch_unwind(move || -> Result<PdfParts, String> {
        let doc = pdf_extract::Document::load_mem(&owned).map_err(|error| format!("not a readable PDF: {error}"))?;
        let info = doc.trailer.get_deref(b"Info", &doc).ok().and_then(|info| info.as_dict().ok());
        let field = |key: &[u8]| {
            info.and_then(|info| info.get_deref(key, &doc).ok())
                .and_then(|object| pdf_extract::decode_text_string(object).ok())
                .map(|text| squash(&text))
                .filter(|text| !text.is_empty())
        };
        let title = field(b"Title");
        let created = field(b"CreationDate");
        let authors = field(b"Author").map(|a| a.split([';', ',']).map(squash).filter(|a| !a.is_empty()).collect()).unwrap_or_default();
        drop(doc);
        let pages = pdf_extract::extract_text_from_mem_by_pages(&owned).map_err(|error| format!("cannot extract the PDF's text: {error}"))?;
        Ok((pages, title, created, authors))
    });
    let (pages, info_title, created, authors) = match outcome {
        Ok(result) => result?,
        Err(panic) => {
            let why = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned())).unwrap_or_default();
            return Err(format!("the PDF extractor failed on this file ({why})"));
        }
    };
    let cleaned: Vec<String> = pages.iter().map(|page| clean_pdf_page(page)).collect();
    let text = cleaned.iter().filter(|page| !page.is_empty()).cloned().collect::<Vec<_>>().join("\n\n");
    if text.trim().is_empty() {
        return Err(format!("the PDF has {} page(s) but no extractable text (a scanned document needs OCR, which is not available)", pages.len()));
    }
    let title = info_title.filter(|t| meaningful_title(t)).unwrap_or_else(|| first_line_title(&text));
    Ok(Extracted {
        title,
        text,
        published: created.as_deref().and_then(normalize_date),
        authors,
        site_name: None,
        pages: Some(pages.len() as u32),
        redirect: None,
    })
}

fn meaningful_title(title: &str) -> bool {
    let lower = title.to_lowercase();
    title.chars().count() >= 4
        && !lower.starts_with("untitled")
        && !lower.starts_with("microsoft word")
        && ![".dvi", ".pdf", ".doc", ".docx", ".tex"].iter().any(|ext| lower.ends_with(ext))
}

fn first_line_title(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| {
            let words = line.split_whitespace().count();
            (2..=30).contains(&words) && !line.to_lowercase().starts_with("arxiv:") && normalize_date(line).is_none() && line.chars().any(char::is_alphabetic)
        })
        .map(|line| crate::text::clip(line, 200))
        .unwrap_or_default()
}

/// Ligatures expanded, hyphenated line ends joined, a paragraph's lines joined, page numbers dropped.
pub fn clean_pdf_page(page: &str) -> String {
    let page: String = page
        .chars()
        .flat_map(|c| match c {
            '\u{FB00}' => "ff".chars().collect::<Vec<_>>(),
            '\u{FB01}' => "fi".chars().collect(),
            '\u{FB02}' => "fl".chars().collect(),
            '\u{FB03}' => "ffi".chars().collect(),
            '\u{FB04}' => "ffl".chars().collect(),
            '\u{00AD}' => Vec::new(),
            '\u{000C}' => vec!['\n'],
            other => vec![other],
        })
        .collect();
    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    for raw in page.lines() {
        let line = squash(raw);
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            continue;
        }
        // A bare page number.
        if line.len() <= 4 && line.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if current.is_empty() {
            current = line;
        } else if current.ends_with('-') && !current.ends_with(" -") && line.chars().next().is_some_and(char::is_lowercase) {
            current.pop();
            current.push_str(&line);
        } else {
            current.push(' ');
            current.push_str(&line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs.join("\n\n")
}

/// Text of a non-HTML, non-PDF textual document: a title from its first line.
pub fn other(text: &str, url: &str) -> Extracted {
    let title = text
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .map(|line| crate::text::clip(line, 120))
        .unwrap_or_else(|| url.rsplit('/').find(|part| !part.is_empty()).unwrap_or(url).to_owned());
    Extracted { title, text: text.trim().to_owned(), ..Extracted::default() }
}
