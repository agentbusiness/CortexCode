//! A tolerant JSON reader for model-written tool arguments.
//!
//! It accepts the mechanical slips models make (trailing commas, single quotes, unquoted keys,
//! Python literals, comments, raw newlines in strings, invalid escapes kept literally, a missing
//! comma between members, missing closing brackets at the very end) and records each repair. It
//! refuses anything whose meaning would be guessed: input cut off inside a string, after a key or
//! after a colon is reported as truncated, never completed.

use serde_json::{Map, Number, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum JsonProblem {
    /// The text ends inside a value whose content cannot be known (string, key, after `:`).
    Truncated { field: Option<String>, detail: String },
    /// Not parseable at all.
    Invalid { position: usize, detail: String },
    /// More than one top-level value (e.g. two tool calls glued together).
    Multiple,
}

pub struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    pub notes: Vec<String>,
    /// Key path of the value being parsed (for error messages).
    path: Vec<String>,
    closed_at_eof: usize,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'-'
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text, bytes: text.as_bytes(), pos: 0, notes: Vec::new(), path: Vec::new(), closed_at_eof: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn note(&mut self, note: impl Into<String>) {
        let note = note.into();
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    fn field(&self) -> Option<String> {
        (!self.path.is_empty()).then(|| self.path.join("."))
    }

    fn truncated(&self, detail: &str) -> JsonProblem {
        JsonProblem::Truncated { field: self.field(), detail: detail.to_owned() }
    }

    fn invalid(&self, detail: impl Into<String>) -> JsonProblem {
        JsonProblem::Invalid { position: self.pos, detail: detail.into() }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    pub fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'/') if self.bytes.get(self.pos + 1) == Some(&b'/') => {
                    self.note("removed // comments");
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.bytes.get(self.pos + 1) == Some(&b'*') => {
                    self.note("removed /* */ comments");
                    match self.text[self.pos + 2..].find("*/") {
                        Some(end) => self.pos += 2 + end + 2,
                        None => self.pos = self.bytes.len(),
                    }
                }
                _ => return,
            }
        }
    }

    pub fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.pos >= self.bytes.len()
    }

    pub fn parse_value(&mut self) -> Result<Value, JsonProblem> {
        self.skip_ws();
        match self.peek() {
            None => Err(self.truncated("the text ends where a value should start")),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"' | b'\'') => self.parse_string().map(Value::String),
            Some(b'-' | b'+' | b'0'..=b'9' | b'.') => self.parse_number(),
            Some(b) if is_ident_start(b) => {
                let start = self.pos;
                while self.peek().is_some_and(is_ident) {
                    self.pos += 1;
                }
                match &self.text[start..self.pos] {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" => Ok(Value::Null),
                    "True" => {
                        self.note("converted Python True/False/None");
                        Ok(Value::Bool(true))
                    }
                    "False" => {
                        self.note("converted Python True/False/None");
                        Ok(Value::Bool(false))
                    }
                    "None" => {
                        self.note("converted Python True/False/None");
                        Ok(Value::Null)
                    }
                    word => {
                        let word = word.to_owned();
                        self.pos = start;
                        Err(self.invalid(format!("unquoted text `{word}` where a value should be")))
                    }
                }
            }
            Some(other) => Err(self.invalid(format!("unexpected character `{}`", other as char))),
        }
    }

    fn parse_object(&mut self) -> Result<Value, JsonProblem> {
        self.pos += 1;
        let mut map = Map::new();
        let mut expect_member = true;
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    self.closed_at_eof += 1;
                    return Ok(Value::Object(map));
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(b'}') {
                        self.note("removed trailing commas");
                    }
                    expect_member = true;
                    continue;
                }
                Some(_) => {
                    if !expect_member {
                        self.note("inserted a missing comma between members");
                    }
                }
            }
            // Key.
            let key = match self.peek() {
                Some(b'"' | b'\'') => {
                    let key_start = self.pos;
                    match self.parse_string() {
                        Ok(key) => key,
                        Err(JsonProblem::Truncated { .. }) => {
                            self.pos = key_start;
                            return Err(self.truncated("the text ends inside an object key"));
                        }
                        Err(other) => return Err(other),
                    }
                }
                Some(b) if is_ident_start(b) => {
                    let start = self.pos;
                    while self.peek().is_some_and(is_ident) {
                        self.pos += 1;
                    }
                    self.note("quoted bare object keys");
                    self.text[start..self.pos].to_owned()
                }
                Some(other) => return Err(self.invalid(format!("expected an object key, found `{}`", other as char))),
                None => return Err(self.truncated("the text ends inside an object")),
            };
            self.skip_ws();
            match self.peek() {
                Some(b':') => self.pos += 1,
                Some(b'=') => {
                    self.note("read `=` as `:` after a key");
                    self.pos += 1;
                }
                None => {
                    self.path.push(key);
                    return Err(self.truncated("the text ends after an object key"));
                }
                Some(other) => return Err(self.invalid(format!("expected `:` after key `{key}`, found `{}`", other as char))),
            }
            self.skip_ws();
            self.path.push(key.clone());
            if self.peek().is_none() {
                return Err(self.truncated("the text ends after `:`, before the value"));
            }
            let value = self.parse_value()?;
            self.path.pop();
            if map.contains_key(&key) {
                return Err(self.invalid(format!("key `{key}` appears twice with possibly different values")));
            }
            map.insert(key, value);
            expect_member = false;
        }
    }

    fn parse_array(&mut self) -> Result<Value, JsonProblem> {
        self.pos += 1;
        let mut items = Vec::new();
        let mut expect_item = true;
        loop {
            self.skip_ws();
            match self.peek() {
                None => {
                    self.closed_at_eof += 1;
                    return Ok(Value::Array(items));
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(b']') {
                        self.note("removed trailing commas");
                    }
                    expect_item = true;
                    continue;
                }
                Some(_) => {
                    if !expect_item {
                        self.note("inserted a missing comma between array items");
                    }
                }
            }
            self.path.push(items.len().to_string());
            let value = self.parse_value()?;
            self.path.pop();
            items.push(value);
            expect_item = false;
        }
    }

    fn parse_string(&mut self) -> Result<String, JsonProblem> {
        let quote = self.bytes[self.pos];
        // Python triple-quoted strings.
        let triple = self.bytes.get(self.pos..self.pos + 3).is_some_and(|run| run.iter().all(|b| *b == quote));
        if triple {
            self.pos += 3;
            let delimiter = std::str::from_utf8(&self.bytes[self.pos - 3..self.pos]).unwrap_or("\"\"\"").to_owned();
            return match self.text[self.pos..].find(&delimiter) {
                Some(end) => {
                    let raw = &self.text[self.pos..self.pos + end];
                    self.pos += end + 3;
                    self.note("read a Python triple-quoted string");
                    Ok(unescape(raw))
                }
                None => Err(self.truncated("the text ends inside a string")),
            };
        }
        if quote == b'\'' {
            self.note("read single-quoted strings");
        }
        self.pos += 1;
        let mut out = String::new();
        let mut notes = Vec::new();
        loop {
            let Some(c) = self.text[self.pos..].chars().next() else {
                return Err(self.truncated("the text ends inside a string"));
            };
            self.pos += c.len_utf8();
            match c {
                c if c as u32 == quote as u32 => break,
                '\\' => {
                    let Some(next) = self.text[self.pos..].chars().next() else {
                        return Err(self.truncated("the text ends inside a string"));
                    };
                    self.pos += next.len_utf8();
                    match next {
                        '"' => out.push('"'),
                        '\'' => out.push('\''),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => {
                            let code = self.read_hex4()?;
                            if (0xD800..0xDC00).contains(&code) && self.text[self.pos..].starts_with("\\u") {
                                self.pos += 2;
                                let low = self.read_hex4()?;
                                let combined = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                                out.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                            } else {
                                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                            }
                        }
                        '\n' => {
                            // Line continuation inside a string: keep the newline as written.
                            out.push('\\');
                            out.push('\n');
                            notes.push("kept an invalid escape sequence literally".to_owned());
                        }
                        other => {
                            out.push('\\');
                            out.push(other);
                            notes.push("kept an invalid escape sequence literally".to_owned());
                        }
                    }
                }
                '\n' | '\t' | '\r' => {
                    notes.push("kept raw control characters inside a string".to_owned());
                    out.push(c);
                }
                c => out.push(c),
            }
        }
        for note in notes {
            self.note(note);
        }
        Ok(out)
    }

    fn read_hex4(&mut self) -> Result<u32, JsonProblem> {
        let hex = self.text.get(self.pos..self.pos + 4).ok_or_else(|| self.truncated("the text ends inside a \\u escape"))?;
        let code = u32::from_str_radix(hex, 16).map_err(|_| self.invalid(format!("invalid \\u escape `{hex}`")))?;
        self.pos += 4;
        Ok(code)
    }

    fn parse_number(&mut self) -> Result<Value, JsonProblem> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        while self.peek().is_some_and(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-' | b'_')) {
            self.pos += 1;
        }
        let raw = &self.text[start..self.pos];
        let cleaned = raw.trim_start_matches('+').replace('_', "");
        if let Ok(integer) = cleaned.parse::<i64>() {
            return Ok(Value::Number(integer.into()));
        }
        if let Ok(unsigned) = cleaned.parse::<u64>() {
            return Ok(Value::Number(unsigned.into()));
        }
        match cleaned.parse::<f64>().ok().and_then(Number::from_f64) {
            Some(number) => Ok(Value::Number(number)),
            None => Err(JsonProblem::Invalid { position: start, detail: format!("`{raw}` is not a number") }),
        }
    }
}

fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some('n') => {
                    out.push('\n');
                    chars.next();
                }
                Some('t') => {
                    out.push('\t');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                Some('\'') => {
                    out.push('\'');
                    chars.next();
                }
                Some('"') => {
                    out.push('"');
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a whole text as one JSON value, tolerantly. Returns the value and the repairs made.
pub fn parse_lenient(text: &str) -> Result<(Value, Vec<String>), JsonProblem> {
    let mut parser = Parser::new(text);
    let value = parser.parse_value()?;
    if parser.closed_at_eof > 0 {
        parser.note(format!("closed {} unterminated bracket(s) at the end", parser.closed_at_eof));
    }
    parser.skip_ws();
    if parser.pos < parser.bytes.len() {
        let rest = parser.rest().trim();
        // Stray closing brackets after a complete value carry no meaning; another value does.
        if rest.bytes().all(|b| matches!(b, b'}' | b']' | b',' | b';' | b' ' | b'\n' | b'\r' | b'\t')) {
            parser.note("removed stray closing brackets after the arguments");
        } else if rest.starts_with('{') || rest.starts_with('[') {
            return Err(JsonProblem::Multiple);
        } else {
            return Err(JsonProblem::Invalid {
                position: parser.pos,
                detail: format!("unexpected text after the arguments: `{}`", rest.chars().take(40).collect::<String>()),
            });
        }
    }
    Ok((value, parser.notes))
}
