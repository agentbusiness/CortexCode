//! Small text helpers shared by sources, extraction, notes and the deep job.

use sha2::{Digest, Sha256};

/// Hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Collapse runs of whitespace into single spaces and trim.
pub fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max` characters (on a character boundary), with `…` when cut.
pub fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
    }
}

/// The first `max` characters and whether anything was cut.
pub fn take_chars(text: &str, max: usize) -> (&str, bool) {
    match text.char_indices().nth(max) {
        None => (text, false),
        Some((at, _)) => (&text[..at], true),
    }
}

/// Characters `from..` of `text` (by character count).
pub fn skip_chars(text: &str, from: usize) -> &str {
    match text.char_indices().nth(from) {
        None => "",
        Some((at, _)) => &text[at..],
    }
}

/// Decode the HTML entities that appear in feeds and result pages (named basics and numeric).
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(end) = tail[..tail.len().min(12)].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Plain text of an HTML fragment (tags dropped, entities decoded, whitespace squashed).
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    squash(&decode_entities(&out))
}

/// A `YYYY-MM-DD` date from the many forms sources use: RFC 3339 / ISO 8601 date-times, plain dates,
/// `YYYY-MM`, `YYYY`, PDF dates (`D:20230518120000Z`), RFC 2822 and Unix seconds.
pub fn normalize_date(text: &str) -> Option<String> {
    let text = text.trim().trim_start_matches("D:");
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(parsed.date_naive().to_string());
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc2822(text) {
        return Some(parsed.date_naive().to_string());
    }
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    // PDF dates and compact forms: 20230518120000Z
    if digits.len() >= 8 && digits.len() != 10 {
        let (y, m, d) = (&digits[0..4], &digits[4..6], &digits[6..8]);
        return valid_ymd(y, m, d);
    }
    if digits.len() == 10 && text.len() == 10 {
        // Unix seconds.
        let seconds: i64 = digits.parse().ok()?;
        return chrono::DateTime::from_timestamp(seconds, 0).map(|time| time.date_naive().to_string());
    }
    let head: String = text.chars().take(10).collect();
    let parts: Vec<&str> = head.split(['-', '/', '.']).collect();
    match parts.as_slice() {
        [y, m, d, ..] if y.len() == 4 => valid_ymd(y, m, &d.chars().take_while(char::is_ascii_digit).collect::<String>()),
        [y, m] if y.len() == 4 && m.len() <= 2 && m.chars().all(|c| c.is_ascii_digit()) => valid_ymd(y, m, "01"),
        [y] if y.len() == 4 && y.chars().all(|c| c.is_ascii_digit()) => Some(format!("{y}-01-01")),
        _ => None,
    }
}

fn valid_ymd(y: &str, m: &str, d: &str) -> Option<String> {
    let (y, m, d): (i32, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    chrono::NaiveDate::from_ymd_opt(y, m, d).map(|date| date.to_string())
}

/// A topic or title as a file name: lowercase ASCII letters and digits joined by `-`, at most 60.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let mut out = out.trim_end_matches('-').to_owned();
    if out.len() > 60 {
        out.truncate(60);
        out = out.trim_end_matches('-').to_owned();
    }
    if out.is_empty() { format!("topic-{}", &sha256_hex(text.as_bytes())[..8]) } else { out }
}

/// Normalize text for quote matching: ligatures expanded, typographic quotes and dashes made plain,
/// hyphenated line breaks joined, whitespace squashed, lowercase.
pub fn match_form(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201F}' | '\u{2033}' => out.push('"'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => out.push('-'),
            '\u{00A0}' | '\u{2009}' | '\u{202F}' => out.push(' '),
            '\u{00AD}' => {}
            _ => out.extend(c.to_lowercase()),
        }
    }
    // "in-\ntroduce" -> "introduce"
    let joined = out.replace("-\n", "").replace("- \n", "");
    squash(&joined)
}

/// Whether `quote` appears in `source` (after [`match_form`] on both); a quote of at least 12
/// words also matches when its first and last eight words both appear in order (a quote with a
/// small mid-sentence omission or a PDF line-break artefact).
pub fn quote_in(quote: &str, source_form: &str) -> bool {
    let normalized = match_form(quote);
    let quote = normalized.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace() || c == '…' || c == '.').to_owned();
    if quote.chars().count() < 8 {
        return false;
    }
    if source_form.contains(&quote) {
        return true;
    }
    let words: Vec<&str> = quote.split(' ').collect();
    if words.len() >= 12 {
        let head = words[..8].join(" ");
        let tail = words[words.len() - 8..].join(" ");
        if let Some(at) = source_form.find(&head) {
            let rest = &source_form[at + head.len()..];
            // The tail must follow within a span not much longer than the quote itself.
            return rest.find(&tail).is_some_and(|gap| gap <= quote.len() * 2);
        }
    }
    false
}
