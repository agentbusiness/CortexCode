//! A decision's free text while its JSON streams (`chatflow.live`).
//!
//! Constrained decisions (ROUTE, DECIDE, CHECKS, ...) are journaled only when complete, so the
//! person would see a fixed stage label for their whole generation. Their top-level string fields
//! are the sentences the model writes for a reader (ROUTE's `work`, DECIDE's `missing`, `brief`,
//! `note`); [`string_fields`] reads them from the JSON written so far, the last one possibly cut
//! off mid-string, and the call publishes them at most every [`EVERY`] (structural: the grammar
//! makes the output one JSON object, so a prefix of it parses up to where it stops).

use std::time::{Duration, Instant};

/// Least interval between two `chatflow.live` events of one call.
pub const EVERY: Duration = Duration::from_millis(250);

/// Top-level string fields of the JSON object `raw` begins (a prefix of one), in the order written:
/// complete ones and the one being written (its text so far). Other values are skipped; nothing
/// when `raw` does not begin an object.
pub fn string_fields(raw: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut chars = raw.chars().peekable();
    skip_ws(&mut chars);
    if chars.next() != Some('{') {
        return fields;
    }
    loop {
        skip_ws(&mut chars);
        match chars.peek() {
            Some(',') => {
                chars.next();
                continue;
            }
            Some('"') => {}
            _ => return fields,
        }
        chars.next();
        let Some((name, true)) = string(&mut chars) else { return fields };
        skip_ws(&mut chars);
        if chars.next() != Some(':') {
            return fields;
        }
        skip_ws(&mut chars);
        match chars.peek() {
            Some('"') => {
                chars.next();
                let Some((text, closed)) = string(&mut chars) else { return fields };
                fields.push((name, text));
                if !closed {
                    return fields;
                }
            }
            Some(_) => {
                if !skip_value(&mut chars) {
                    return fields;
                }
            }
            None => return fields,
        }
    }
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn skip_ws(chars: &mut Chars<'_>) {
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
}

/// The rest of a string after its opening quote, decoded: (text, closed). None on a malformed escape.
fn string(chars: &mut Chars<'_>) -> Option<(String, bool)> {
    let mut text = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some((text, true)),
            '\\' => {
                let Some(escape) = chars.next() else { return Some((text, false)) };
                match escape {
                    '"' => text.push('"'),
                    '\\' => text.push('\\'),
                    '/' => text.push('/'),
                    'b' => text.push('\u{8}'),
                    'f' => text.push('\u{c}'),
                    'n' => text.push('\n'),
                    'r' => text.push('\r'),
                    't' => text.push('\t'),
                    'u' => {
                        let hex: String = chars.by_ref().take(4).collect();
                        if hex.len() < 4 {
                            return Some((text, false));
                        }
                        let unit = u32::from_str_radix(&hex, 16).ok()?;
                        if (0xD800..0xDC00).contains(&unit) {
                            // A surrogate pair: the low half follows as another escape.
                            let rest: String = chars.by_ref().take(6).collect();
                            if rest.len() < 6 {
                                return Some((text, false));
                            }
                            let low = rest.strip_prefix("\\u").and_then(|hex| u32::from_str_radix(hex, 16).ok())?;
                            text.push(char::from_u32(0x10000 + ((unit - 0xD800) << 10) + (low.checked_sub(0xDC00)?))?);
                        } else {
                            text.push(char::from_u32(unit)?);
                        }
                    }
                    _ => return None,
                }
            }
            c => text.push(c),
        }
    }
    Some((text, false))
}

/// Skip one non-string value (number, literal, array, object). False when it is not complete yet.
fn skip_value(chars: &mut Chars<'_>) -> bool {
    let mut depth = 0usize;
    while let Some(&c) = chars.peek() {
        match c {
            '"' => {
                chars.next();
                match string(chars) {
                    Some((_, true)) => {}
                    _ => return false,
                }
                continue;
            }
            '[' | '{' => depth += 1,
            ']' | '}' => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
                if depth == 0 {
                    chars.next();
                    return true;
                }
            }
            ',' if depth == 0 => return true,
            _ => {}
        }
        chars.next();
    }
    false
}

/// When the next event of a call is due.
#[derive(Debug, Default)]
pub struct Throttle {
    last: Option<Instant>,
    sent: Vec<(String, String)>,
}

impl Throttle {
    /// The fields to publish now: changed since the last event and at least [`EVERY`] after it.
    pub fn due(&mut self, fields: Vec<(String, String)>, now: Instant) -> Option<Vec<(String, String)>> {
        if fields.is_empty() || fields == self.sent || self.last.is_some_and(|last| now.duration_since(last) < EVERY) {
            return None;
        }
        self.last = Some(now);
        self.sent = fields.clone();
        Some(fields)
    }
}
